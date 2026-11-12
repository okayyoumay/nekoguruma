// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_PDU_API_ROOT_FILE overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! The agent keeps a job's session alive end to end through the D-PDU API worker (design 8.2.5,
//! ADR-418, ADR-420, ADR-423): the agent runs a procedure against a real `iso22900-service`
//! worker and `iso22900-mock`, whose simulated ECU drops a non-default session after 1.5 s
//! without a request (`s3_server_ms`). It mirrors
//! `crates/j2534-0404-service/tests/agent_keep_alive.rs`, which runs the same procedures through
//! the J2534 worker and `sim-vci`.
//!
//! The procedure enters the programming session, waits 4 s, and then sends a request that exists
//! only in that session (the check programming dependencies routine, which answers a request
//! sequence error there and request out of range in the default session). With the keep-alive on,
//! the link's first session control goes as the request of a `CoptStartcomm`, which starts the
//! mock's cyclic tester-present, so the session outlasts the wait; with it off, the session
//! lapses during the wait. The keep-alive period is 500 ms here, under the ECU's timeout.
//!
//! An ECU that stops answering the tester present ends the run (ADR-418 item 6, ADR-420 item 8):
//! a second worker is started with `NGR_SIM_ECU_COMMANDS` set to drop the response to the first
//! TesterPresent, which the mock reports as `PDU_ERR_EVT_TESTER_PRESENT_ERROR`. The agent reads
//! it at the next poll of the procedure's `Wait`, so the run ends in `SessionLost` before the
//! routine is sent.
//!
//! This file holds a single test, so the process-wide environment it sets for the spawned
//! services cannot race with another test.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::{
    CommLogSetup, CommLogStatus, HostError, Job, JobError, JobLimits, JobRun, LinkConfig,
    run_program,
};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Value};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "NGR Mock D-PDU API";
/// The simulated ECU's check-programming-dependencies routine (`sim_ecu::RID_CHECK_PROGRAMMING_DEPENDENCIES`).
const RID_CHECK_PROGRAMMING_DEPENDENCIES: u16 = 0xFF01;
/// How long the procedure waits after entering the session: longer than the ECU's timeout.
const WAIT_MS: u32 = 4_000;
/// The keep-alive period of the jobs that keep a session: under the ECU's 1.5 s timeout.
const PERIOD: Duration = Duration::from_millis(500);

/// Enters the programming session with a session control, waits [`WAIT_MS`], and starts the
/// check programming dependencies routine.
fn session_program() -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x10 },
            Op::Wait { millis: WAIT_MS },
            Op::PushBytes(1),
            Op::RoutineControl {
                routine: RID_CHECK_PROGRAMMING_DEPENDENCIES,
                sub: 0x01,
            },
        ],
        constants: vec![vec![0x02], Vec::new()],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
        targets: Vec::new(),
    }
}

/// Guards for one job, in a lock directory of this call's own.
fn job_guards() -> JobGuards {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let setup = GuardSetup {
        dir: std::env::temp_dir().join(format!(
            "agent-keep-alive-dpdu-locks-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )),
        vci: "agent-keep-alive-dpdu-vci".to_owned(),
    };
    let never = AtomicBool::new(false);
    JobGuards::take(&setup, Duration::from_millis(10), &never).expect("guards should be free")
}

#[test]
fn the_agent_keeps_a_session_alive_across_a_wait_through_the_d_pdu_worker() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let root = temp.path();
    let config_path = root.join("config.toml");
    let ecu_path = root.join("ecu.json");
    let comm_log_dir = root.join("commlog");
    std::fs::write(&config_path, "").expect("test config file should be writable");
    std::fs::create_dir_all(&comm_log_dir).expect("test directory");
    let library = iso22900_mock::mock_library_path().expect("the mock cdylib should be built");
    let root_file = iso22900_mock::write_root_file(root, LIBRARY_NAME, &library)
        .expect("root description file should be writable");
    // No security access; a non-default session lasts 1.5 s without a request.
    std::fs::write(
        &ecu_path,
        r#"{"vin": "NGRSIMECU00000004", "part_number": "NGR-SIM-ECU", "sw_version": "1.0.0",
            "response_delay_ms": 0, "drop_at_block": null, "require_security_access": false,
            "require_gateway_auth": false, "fail_checksum": false, "s3_server_ms": 1500}"#,
    )
    .expect("ECU config file should be writable");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::set_var("NGR_PDU_API_ROOT_FILE", &root_file);
        std::env::set_var(iso22900_mock::ECU_CONFIG_ENV, &ecu_path);
        std::env::remove_var(iso22900_mock::ECU_COMMANDS_ENV);
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
            tokio::time::timeout(Duration::from_secs(180), run_the_jobs(&comm_log_dir)).await
        })
        .expect("the flow should finish in time");
}

/// What a job returned: its stack, and the lines of its communication log.
struct Outcome {
    stack: Vec<Value>,
    /// How the job ended, when it did not finish.
    error: Option<JobError>,
    log: Vec<serde_json::Value>,
}

impl Outcome {
    /// The outcome of a job that finished.
    fn finished(self) -> Self {
        assert!(
            self.error.is_none(),
            "the job should finish: {:?} {:?}",
            self.error,
            self.log
        );
        self
    }
}

async fn launch() -> WorkerProcess {
    WorkerProcess::launch(
        Path::new(env!("CARGO_BIN_EXE_iso22900-service")),
        ServiceKind::Iso22900,
        LIBRARY_NAME,
        OperatingMode::User,
        // Generous timeouts for a debug binary on a busy CI runner.
        &LaunchOptions {
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(5),
            ..LaunchOptions::default()
        },
    )
    .await
    .expect("worker should launch")
}

/// Runs `procedure` on `worker` with the keep-alive period `keep_alive`.
async fn run(
    worker: &WorkerProcess,
    comm_log_dir: &Path,
    job_number: u32,
    procedure: Program,
    keep_alive: Option<Duration>,
) -> Outcome {
    let setup = CommLogSetup {
        dir: comm_log_dir.to_owned(),
        key: agent::journal::JobKey {
            job_id: shared_proto::JobId(format!(
                "0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5f{job_number:02}"
            )),
            generation: 1,
        },
    };
    // A job without a document opens the link its caller gives, which is how the test sets the
    // period (a document's link has the default).
    let job = Job::from(procedure).with_comm_log(setup.clone());
    let link = LinkConfig {
        keep_alive,
        ..LinkConfig::iso15765(0x7E0, 0x7E8)
    };
    let JobRun {
        result,
        guards: _guards,
        comm_log,
    } = run_program(
        worker
            .connect(&ConnectOptions::default())
            .await
            .expect("client should connect"),
        &link,
        job,
        JobLimits::default(),
        job_guards(),
    )
    .await;
    let (stack, error) = match result {
        Ok(state) => (state.stack, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    assert_eq!(comm_log, CommLogStatus::Complete);
    let text = std::fs::read_to_string(setup.path().expect("the log's path")).expect("the log");
    Outcome {
        stack,
        error,
        log: text
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON line"))
            .collect(),
    }
}

async fn run_the_jobs(comm_log_dir: &Path) {
    let worker = launch().await;

    // Kept alive: the session outlasts the wait, so the routine answers a request sequence error,
    // as it does in the programming session.
    let kept = run(&worker, comm_log_dir, 1, session_program(), Some(PERIOD))
        .await
        .finished();
    let [Value::Bytes(session), Value::Bytes(routine)] = kept.stack.as_slice() else {
        panic!("{:?}", kept.stack);
    };
    assert!(session.starts_with(&[0x50, 0x02]), "{:?}", kept.stack);
    assert_eq!(routine, &[0x7F, 0x31, 0x24], "{:?}", kept.stack);
    // The session control went as a StartComm, and the tester present is on from it.
    let exchange = kept
        .log
        .iter()
        .find(|line| line["event"] == "exchange")
        .expect("the session control's exchange");
    assert_eq!(exchange["detail"]["cop_type"], "startcomm", "{exchange}");
    assert_eq!(exchange["outcome"]["result"], "positive", "{exchange}");
    let alive: Vec<&serde_json::Value> = kept
        .log
        .iter()
        .filter(|line| line["event"] == "keep_alive")
        .collect();
    assert_eq!(alive.len(), 1, "{:?}", kept.log);
    assert_eq!(alive[0]["origin"], "keep_alive");
    assert_eq!(alive[0]["action"], "on");
    assert_eq!(alive[0]["means"], "start_comm");
    assert_eq!(alive[0]["result"], "ok");
    // The routine, a plain send-receive, has no `cop_type`.
    let routine_exchange = kept
        .log
        .iter()
        .rfind(|line| line["event"] == "exchange")
        .expect("the routine's exchange");
    assert!(routine_exchange["detail"].get("cop_type").is_none());

    // The same job with the keep-alive off: the session lapsed during the wait, so the routine is
    // out of range, as in the default session.
    let lapsed = run(&worker, comm_log_dir, 2, session_program(), None)
        .await
        .finished();
    let [Value::Bytes(_), Value::Bytes(routine)] = lapsed.stack.as_slice() else {
        panic!("{:?}", lapsed.stack);
    };
    assert_eq!(routine, &[0x7F, 0x31, 0x31], "{:?}", lapsed.stack);
    assert!(
        lapsed.log.iter().all(|line| line["event"] != "keep_alive"),
        "{:?}",
        lapsed.log
    );
    let exchange = lapsed
        .log
        .iter()
        .find(|line| line["event"] == "exchange")
        .expect("the session control's exchange");
    assert!(exchange["detail"].get("cop_type").is_none(), "{exchange}");
    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("the worker stops");

    // An ECU that stops answering the tester present: the mock applies `NGR_SIM_ECU_COMMANDS` when
    // the service constructs it, so a second worker, started with the variable set, loses the
    // response to the first TesterPresent that reaches the ECU, which is the mock's first, sent
    // as the session starts. The mock reports its tester-present error event; the agent reads
    // it at its next poll of the wait, and the run ends before the routine is sent.
    // SAFETY: the only threads are the runtime's, which do not read the environment; the
    // variable is set around the launch, which copies it into the worker's environment.
    unsafe {
        std::env::set_var(
            iso22900_mock::ECU_COMMANDS_ENV,
            r#"[{"inject_fault":{"fault":"drop_response","service":62}}]"#,
        );
    }
    let worker = launch().await;
    // SAFETY: as above.
    unsafe { std::env::remove_var(iso22900_mock::ECU_COMMANDS_ENV) };
    let lost = run(&worker, comm_log_dir, 3, session_program(), Some(PERIOD)).await;
    match &lost.error {
        Some(JobError::Host {
            source: HostError::SessionLost,
            ..
        }) => {}
        other => panic!(
            "the run should end in SessionLost: {other:?} {:?}",
            lost.log
        ),
    }
    assert!(lost.stack.is_empty());
    let errors: Vec<&serde_json::Value> = lost
        .log
        .iter()
        .filter(|line| line["event"] == "keep_alive" && line["action"] == "error")
        .collect();
    assert_eq!(errors.len(), 1, "{:?}", lost.log);
    assert_eq!(errors[0]["origin"], "keep_alive");
    assert_eq!(errors[0]["link_error"], "tester_present");
    assert_eq!(errors[0]["session_lost"], true);
    // The D-PDU worker names the mock's event as the J2534 one names its own.
    assert_eq!(
        errors[0]["detail"]["error_event"],
        "PDU_ERR_EVT_TESTER_PRESENT_ERROR"
    );
    // Only the session control was sent: the wait ended the run before the routine.
    let exchanges: Vec<&serde_json::Value> = lost
        .log
        .iter()
        .filter(|line| line["event"] == "exchange")
        .collect();
    assert_eq!(exchanges.len(), 1, "{:?}", lost.log);
    assert_eq!(exchanges[0]["detail"]["cop_type"], "startcomm");
    assert!(
        errors[0]["seq"].as_u64() > exchanges[0]["seq"].as_u64(),
        "{:?}",
        lost.log
    );
    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("the worker stops");
}

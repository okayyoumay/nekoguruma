// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! The agent keeps a job's session alive end to end (design 8.2.5, ADR-418, ADR-420): the agent
//! runs a procedure against a real `j2534-0404-service` worker and `sim-vci`, whose simulated
//! ECU drops a non-default session after 1.5 s without a request (`s3_server_ms`).
//!
//! The procedure enters the programming session, waits 4 s, and then sends a request that exists
//! only in that session (the check programming dependencies routine, which answers a request
//! sequence error there and request out of range in the default session). With the keep-alive on,
//! the link's first session control goes as the request of a `CoptStartcomm`, which starts the
//! worker's cyclic tester-present, so the session outlasts the wait; with it off, the session
//! lapses during the wait. The keep-alive period is 500 ms here, under the ECU's timeout (the
//! default 2 s is longer than this test ECU's 1.5 s).
//!
//! The communication log shows the session control as an exchange with `cop_type` `startcomm`,
//! then a `keep_alive` entry with the tester present on. A session control whose positive
//! response is suppressed (`10 82`) goes the same way: the ECU's silence is its result (ADR-408),
//! so the worker's receive timeout is reported for a StartComm's receive phase, and the session
//! is kept as well. Returning to the default session switches the tester present off by
//! `CoptUpdateparam`.
//!
//! This file holds a single test, so the process-wide environment it sets for the spawned service
//! cannot race with another test.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::{CommLogSetup, CommLogStatus, Job, JobLimits, JobRun, LinkConfig, run_program};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Value};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's check-programming-dependencies routine (`sim_ecu::RID_CHECK_PROGRAMMING_DEPENDENCIES`).
const RID_CHECK_PROGRAMMING_DEPENDENCIES: u16 = 0xFF01;
/// How long the procedure waits after entering the session: longer than the ECU's timeout.
const WAIT_MS: u32 = 4_000;
/// The keep-alive period of the jobs that keep a session: under the ECU's 1.5 s timeout.
const PERIOD: Duration = Duration::from_millis(500);

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

/// Removes the temporary file or directory however the test ends.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Enters `session` with a session control (a sub-function the test chooses, so `0x82` suppresses
/// the positive response), waits [`WAIT_MS`], and starts the check programming dependencies
/// routine.
fn session_program(session: u8) -> Program {
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
        constants: vec![vec![session], Vec::new()],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
        targets: Vec::new(),
    }
}

/// Enters the programming session and returns to the default one.
fn there_and_back_program() -> Program {
    Program {
        code: vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x10 },
            Op::PushBytes(1),
            Op::ServiceRequest { service: 0x10 },
        ],
        constants: vec![vec![0x02], vec![0x01]],
        ..session_program(0x02)
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
            "agent-keep-alive-locks-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )),
        vci: "agent-keep-alive-vci".to_owned(),
    };
    let never = AtomicBool::new(false);
    JobGuards::take(&setup, Duration::from_millis(10), &never).expect("guards should be free")
}

#[test]
fn the_agent_keeps_a_session_alive_across_a_wait() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let pid = std::process::id();
    let tmp =
        |name: &str| std::env::temp_dir().join(format!("agent-keep-alive-{pid}-{nanos}-{name}"));
    let config_path = tmp("sim-vci.toml");
    let ecu_path = tmp("ecu.json");
    let definition_dir = tmp("definitions");
    let comm_log_dir = tmp("commlog");
    let _temps = [
        Temp(config_path.clone()),
        Temp(ecu_path.clone()),
        Temp(definition_dir.clone()),
        Temp(comm_log_dir.clone()),
    ];
    std::fs::write(&config_path, "").expect("test config file should be writable");
    for dir in [&definition_dir, &comm_log_dir] {
        std::fs::create_dir_all(dir).expect("test directory");
    }
    // The registration definition names the library, through the debug-only
    // `NGR_J2534_DEFINITION_DIR` override (ADR-282).
    j2534_0404_mock::write_definition(&definition_dir, LIBRARY_NAME, &sim_vci_path())
        .expect("definition file should be writable");
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
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &definition_dir);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        std::env::set_var("NGR_SIM_ECU_CONFIG", &ecu_path);
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
            tokio::time::timeout(Duration::from_secs(120), run_the_jobs(&comm_log_dir)).await
        })
        .expect("the flow should finish in time");
}

/// What a job returned: its stack, and the lines of its communication log.
struct Outcome {
    stack: Vec<Value>,
    log: Vec<serde_json::Value>,
}

async fn run_the_jobs(comm_log_dir: &std::path::Path) {
    let worker = WorkerProcess::launch(
        std::path::Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
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

    let mut next = 0;
    let mut run = |procedure: Program, keep_alive: Option<Duration>| {
        next += 1;
        let setup = CommLogSetup {
            dir: comm_log_dir.to_owned(),
            key: agent::journal::JobKey {
                job_id: shared_proto::JobId(format!("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5f{next:02}")),
                generation: 1,
            },
        };
        let worker = &worker;
        async move {
            // A job without a document opens the link its caller gives, which is how the test
            // sets the period (a document's link has the default).
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
            let state = result.expect("the job should finish");
            assert_eq!(comm_log, CommLogStatus::Complete);
            let text =
                std::fs::read_to_string(setup.path().expect("the log's path")).expect("the log");
            Outcome {
                stack: state.stack,
                log: text
                    .lines()
                    .map(|line| serde_json::from_str(line).expect("a JSON line"))
                    .collect(),
            }
        }
    };

    // Kept alive: the session outlasts the wait, so the routine answers a request sequence error,
    // as it does in the programming session.
    let kept = run(session_program(0x02), Some(PERIOD)).await;
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
    let lapsed = run(session_program(0x02), None).await;
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

    // A session control whose positive response is suppressed: the ECU's silence is its result
    // (ADR-408), reported by the worker's receive timeout of the StartComm, and the session is
    // kept all the same.
    let silent = run(session_program(0x82), Some(PERIOD)).await;
    let [Value::Bytes(session), Value::Bytes(routine)] = silent.stack.as_slice() else {
        panic!("{:?}", silent.stack);
    };
    assert!(session.is_empty(), "{:?}", silent.stack);
    assert_eq!(routine, &[0x7F, 0x31, 0x24], "{:?}", silent.stack);
    let exchange = silent
        .log
        .iter()
        .find(|line| line["event"] == "exchange")
        .expect("the session control's exchange");
    assert_eq!(exchange["detail"]["cop_type"], "startcomm", "{exchange}");
    assert_eq!(exchange["outcome"]["result"], "no_response", "{exchange}");
    assert!(
        silent.log.iter().any(|line| line["event"] == "keep_alive"
            && line["action"] == "on"
            && line["means"] == "start_comm"),
        "{:?}",
        silent.log
    );

    // Back to the default session: the tester present is switched off by CoptUpdateparam.
    let back = run(there_and_back_program(), Some(PERIOD)).await;
    let switches: Vec<(&str, &str, &str)> = back
        .log
        .iter()
        .filter(|line| line["event"] == "keep_alive")
        .map(|line| {
            (
                line["action"].as_str().expect("an action"),
                line["means"].as_str().expect("a means"),
                line["result"].as_str().expect("a result"),
            )
        })
        .collect();
    assert_eq!(
        switches,
        [("on", "start_comm", "ok"), ("off", "update_param", "ok")],
        "{:?}",
        back.log
    );
}

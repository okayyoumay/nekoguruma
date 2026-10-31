// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! An identification job's functional presence request end to end (ADR-327 item 2, ADR-396):
//! `worker-host` launches the real `j2534-0404-service` binary against the `sim-vci` cdylib, and
//! the agent runs an identification plan over the example ECU's document, whose protocol
//! declares the functional identifier 0x7DF. The request leaves on a functionally addressed link
//! whose receive table names the example ECU's pair 0x7E0 / 0x7E8, the J2534 worker collects
//! every answer within the window, and the report says which response identifier answered.
//! One scenario each: the simulated ECU answers, is silent (the answer is dropped), answers
//! negatively, answers after a chain of pendings, and answers after the window has ended.
//!
//! This file holds a single test, so the process-wide environment it sets for the spawned
//! service cannot race with another test.

#[expect(
    dead_code,
    reason = "the other end-to-end tests use the rest of the fixture"
)]
#[path = "../../diag-frontend/tests/support/definition_end_to_end.rs"]
mod definition;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::identification::{
    CheckedPlan, IdentificationRun, check_plan, load_plan, run_identification,
};
use agent::{CommLogSetup, CommLogStatus, JobDocument, JobLimits};
use diag_ir::plan::{IdentificationPlan, PLAN_SCHEMA_VERSION, PlanDefinition, Selection};
use worker_host::client::{ConnectOptions, WorkerClient};
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's response identifier, from the example ECU's protocol.
const ECU_RESPONSE_ID: u32 = 0x7E8;
/// TesterPresent's service identifier, as `inject_fault` takes it.
const TESTER_PRESENT: u8 = 0x3E;
/// The ECU's delay of an answer that comes after the window: past the example ECU's P2 of one
/// second with room to spare.
const LATE_MS: u32 = 3_000;
/// The longest the test waits for the whole flow.
const FLOW_TIMEOUT: Duration = Duration::from_secs(180);

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

/// Writes `command` as the control file `name`, under a temporary name first so `sim-vci`
/// never reads it half-written. It takes effect at the next J2534 call, in name order.
fn send(control_dir: &Path, name: &str, command: &str) {
    let tmp = control_dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, command).expect("control file should be writable");
    std::fs::rename(&tmp, control_dir.join(format!("{name}.json")))
        .expect("control file should be renamed");
}

/// A plan that reads the example ECU's first variant: every ECU is in scope, so the plan holds
/// one definition and no candidate rule.
fn checked_plan() -> CheckedPlan {
    let (_, document) = definition::documents("read.star")
        .into_iter()
        .next()
        .expect("the example ECU has a variant");
    let hash = JobDocument::new(document.clone())
        .expect("the document loads")
        .sha256();
    let plan = IdentificationPlan {
        schema_version: PLAN_SCHEMA_VERSION,
        selection: Selection::EveryEcu,
        candidates: Vec::new(),
        definitions: vec![PlanDefinition {
            id: "example-ecu".to_owned(),
            documents: vec![hash],
        }],
    };
    let loaded = load_plan(
        &plan.encode().expect("the plan encodes"),
        vec![(hash, document)],
    )
    .expect("the plan loads");
    check_plan(&loaded).expect("the plan passes its checks")
}

async fn launch_worker() -> (WorkerProcess, WorkerClient) {
    let worker = WorkerProcess::launch(
        Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        ServiceKind::J2534V0404,
        LIBRARY_NAME,
        OperatingMode::User,
        &LaunchOptions {
            // Generous timeouts for a debug binary on a busy CI runner.
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
    (worker, client)
}

/// The log's lines that parse.
fn log_lines(setup: &CommLogSetup) -> Vec<serde_json::Value> {
    std::fs::read_to_string(setup.path().expect("the log's path"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// What one scenario found.
struct Found {
    run: IdentificationRun,
    log: Vec<serde_json::Value>,
}

/// Runs the plan once on `client`, with the fault `fault` armed first if there is one.
async fn run_once(
    client: &WorkerClient,
    root: &Path,
    control_dir: &Path,
    name: &str,
    fault: Option<&str>,
) -> Found {
    if let Some(fault) = fault {
        send(
            control_dir,
            &format!("{name}-fault"),
            &format!(
                r#"{{"command": "inject_fault", "service": {TESTER_PRESENT}, "fault": {fault}}}"#
            ),
        );
    }
    let setup = CommLogSetup::for_run(root.join("log").join(name)).expect("log directory");
    let guards = JobGuards::take_vci_only(
        &GuardSetup {
            dir: root.join("locks"),
            vci: "sim-presence-vci".to_owned(),
        },
        Duration::from_millis(10),
        &AtomicBool::new(false),
    )
    .expect("guards should be taken");
    let run = run_identification(
        client.clone(),
        checked_plan(),
        JobLimits::default(),
        guards,
        Some(setup.clone()),
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    let log = log_lines(&setup);
    Found { run, log }
}

impl Found {
    /// The report, with the run's bookkeeping checked: one presence link, no refused table, no
    /// interruption, the link closed and the log complete.
    fn report(&self, what: &str) -> &agent::identification::IdentificationReport {
        let report = self
            .run
            .result
            .as_ref()
            .unwrap_or_else(|error| panic!("{what}: {error}\n{:#?}", self.log));
        assert_eq!(report.presence_links_opened(), 1, "{what}");
        assert!(report.refused_tables().is_empty(), "{what}");
        assert!(!report.presence_interrupted(), "{what}");
        assert!(!self.run.guards.link_unconfirmed(), "{what}");
        assert_eq!(self.run.comm_log, CommLogStatus::Complete, "{what}");
        report
    }

    fn heard(&self, what: &str) -> Vec<(u32, String)> {
        self.report(what)
            .presence()
            .iter()
            .map(|answer| {
                let bytes = answer.bytes.iter().map(|b| format!("{b:02x}")).collect();
                (answer.rx_id, bytes)
            })
            .collect()
    }

    /// The presence exchange's entry: the only exchange of the run.
    fn exchange(&self, what: &str) -> &serde_json::Value {
        let exchanges: Vec<_> = self
            .log
            .iter()
            .filter(|line| line["event"] == "exchange")
            .collect();
        assert_eq!(exchanges.len(), 1, "{what}\n{:#?}", self.log);
        exchanges[0]
    }
}

#[test]
fn the_presence_request_collects_what_the_simulated_ecu_answers() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let pid = std::process::id();
    let root = tempfile::tempdir().expect("temporary directory");
    let temp = std::env::temp_dir();
    let config = temp.join(format!("sim-vci-presence-{pid}-{nanos}.toml"));
    let control_dir = temp.join(format!("sim-vci-presence-{pid}-{nanos}"));
    let definitions = temp.join(format!("sim-vci-presence-definitions-{pid}-{nanos}"));
    std::fs::write(&config, "").expect("test config file should be writable");
    // The registration definition names the library, through the debug-only
    // `NGR_J2534_DEFINITION_DIR` override (ADR-282).
    std::fs::create_dir_all(&definitions).expect("definition directory");
    j2534_0404_mock::write_definition(&definitions, LIBRARY_NAME, &sim_vci_path())
        .expect("definition file should be writable");
    std::fs::create_dir(&control_dir).expect("control directory should be created");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config);
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &definitions);
        std::env::set_var("NGR_SIM_VCI_CONTROL_DIR", &control_dir);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        std::env::remove_var("NGR_SIM_ECU_CONFIG");
        std::env::remove_var("NGR_SIM_ECU_STATE");
    }
    // The job's thread blocks on the runtime for every primitive, so it must be multi-threaded.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    // A wedged service must fail the test, not hang the CI job.
    runtime
        .block_on(async {
            tokio::time::timeout(FLOW_TIMEOUT, flow(root.path(), &control_dir)).await
        })
        .expect("the end-to-end flow should finish in time");
    let _ = std::fs::remove_file(&config);
    let _ = std::fs::remove_dir_all(&control_dir);
    let _ = std::fs::remove_dir_all(&definitions);
}

async fn flow(root: &Path, control_dir: &Path) {
    let (worker, client) = launch_worker().await;
    let positive = || vec![(ECU_RESPONSE_ID, "7e00".to_owned())];

    // The ECU answers: one answer, on the example ECU's pair, on a functional link.
    let found = run_once(&client, root, control_dir, "answer", None).await;
    assert_eq!(found.heard("answer"), positive());
    let report = found.report("answer");
    assert!(
        report
            .endpoints()
            .iter()
            .all(|endpoint| report.answered(&endpoint.key))
    );
    let exchange = found.exchange("answer");
    assert_eq!(exchange["origin"], "identification");
    assert_eq!(exchange["target"]["tx_id"], 0x7DF);
    assert_eq!(
        exchange["answers"].as_array().map(Vec::len),
        Some(1),
        "{exchange}"
    );
    assert_eq!(exchange["answers"][0]["rx_id"], ECU_RESPONSE_ID);
    assert_eq!(exchange["outcome"]["result"], "positive");
    let opened = found
        .log
        .iter()
        .find(|line| line["event"] == "link_opened")
        .unwrap();
    assert_eq!(opened["functional"]["functional_id"], 0x7DF);
    assert_eq!(
        opened["functional"]["table"],
        serde_json::json!([{"request_id": 0x7E0, "response_id": 0x7E8}])
    );

    // The run's guards hold the VCI's lock until they are dropped.
    drop(found);

    // The answer is dropped: the window ends with none, and nothing is dropped from the plan.
    let found = run_once(
        &client,
        root,
        control_dir,
        "silent",
        Some(r#""drop_response""#),
    )
    .await;
    assert_eq!(found.heard("silent"), []);
    let report = found.report("silent");
    assert!(
        report
            .endpoints()
            .iter()
            .all(|endpoint| !report.answered(&endpoint.key))
    );
    assert_eq!(found.exchange("silent")["outcome"]["result"], "no_response");

    // The run's guards hold the VCI's lock until they are dropped.
    drop(found);

    // A negative answer is presence too, with its code.
    let negative = r#"{"negative_response": {"nrc": "conditions_not_correct"}}"#;
    let found = run_once(&client, root, control_dir, "negative", Some(negative)).await;
    assert_eq!(
        found.heard("negative"),
        [(ECU_RESPONSE_ID, "7f3e22".to_owned())]
    );
    assert_eq!(found.exchange("negative")["outcome"]["result"], "negative");

    // The run's guards hold the VCI's lock until they are dropped.
    drop(found);

    // A chain of pendings, then the final answer: one answer.
    let pending = r#"{"response_pending": {"count": 2, "interval_ms": 300}}"#;
    let found = run_once(&client, root, control_dir, "pending", Some(pending)).await;
    assert_eq!(found.heard("pending"), positive());
    assert_eq!(
        found.exchange("pending")["answers"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    // The run's guards hold the VCI's lock until they are dropped.
    drop(found);

    // An answer after the window has ended is not presence.
    let late = format!(r#"{{"delay_response": {{"ms": {LATE_MS}}}}}"#);
    let found = run_once(&client, root, control_dir, "late", Some(&late)).await;
    assert_eq!(found.heard("late"), []);
    assert_eq!(found.exchange("late")["outcome"]["result"], "no_response");

    drop(found);
    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}

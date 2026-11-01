// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! An identification job's functional presence request end to end (ADR-327 item 2, ADR-396):
//! `worker-host` launches the real `j2534-0404-service` binary against the `sim-vci` cdylib, and
//! the agent runs an identification plan over the example ECU's document, whose protocol
//! declares the functional identifier 0x7DF. The request leaves on a functionally addressed link
//! whose receive table names the example ECU's pair 0x7E0 / 0x7E8, the J2534 worker collects
//! every answer within the window, and the report says which response identifier answered.
//! One scenario each: the simulated ECU answers, is silent (the answer is dropped), answers
//! negatively, answers after a chain of pendings, answers twice (ADR-400), and answers after
//! the window has ended.
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
    CheckedPlan, FieldValue, IdentificationRun, RequestOutcome, check_plan, load_plan,
    run_identification,
};
use agent::launch::Api;
use agent::{CommLogSetup, CommLogStatus, JobDocument, JobLimits, VciFacts};
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

/// A variant's document that declares only its link: UDS over ISO 15765 at 500 kbit/s on
/// `tx_id` / `rx_id`, with the example ECU's functional identifier.
fn link_document(tx_id: u32, rx_id: u32) -> Vec<u8> {
    use diag_ir::declaration::{EcuDocumentT, EcuVariantT, ProtocolT, encode_ecu_document};
    let mut protocol = ProtocolT::default();
    protocol.short_name = Some("ISO15765".into());
    protocol.application_layer = Some("UDS".into());
    protocol.baud_rate = 500_000;
    protocol.tx_id = tx_id;
    protocol.rx_id = rx_id;
    protocol.functional_id = 0x7DF;
    let mut variant = EcuVariantT::default();
    variant.id = 1;
    variant.short_name = Some(format!("ecu-{tx_id:x}-{rx_id:x}"));
    variant.protocols = Some(vec![protocol]);
    let mut document = EcuDocumentT::default();
    document.schema_version = diag_ir::IR_SCHEMA_VERSION;
    document.source_format = Some("csv-starlark".into());
    document.source_digest = Some("00".into());
    document.variant = Some(Box::new(variant));
    encode_ecu_document(&document)
}

/// A plan that reads one ECU per pair of `pairs`, declared in that order, all on the one bus.
fn plan_over(pairs: &[(u32, u32)]) -> CheckedPlan {
    let documents: Vec<_> = pairs
        .iter()
        .map(|&(tx_id, rx_id)| {
            let bytes = link_document(tx_id, rx_id);
            let hash = JobDocument::new(bytes.clone())
                .expect("the document loads")
                .sha256();
            (hash, bytes)
        })
        .collect();
    let plan = IdentificationPlan {
        schema_version: PLAN_SCHEMA_VERSION,
        selection: Selection::EveryEcu,
        candidates: Vec::new(),
        definitions: vec![PlanDefinition {
            id: "ecus".to_owned(),
            documents: documents.iter().map(|(hash, _)| *hash).collect(),
        }],
    };
    let loaded =
        load_plan(&plan.encode().expect("the plan encodes"), documents).expect("the plan loads");
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
    plan: CheckedPlan,
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
        plan,
        JobLimits::default(),
        guards,
        Some(setup.clone()),
        Some(VciFacts {
            api: Api::J2534,
            long_size: long_size(),
        }),
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    let log = log_lines(&setup);
    Found { run, log }
}

/// An answer item of a presence entry that is placed in time: on the device clock, with its
/// epoch. It has no response time: the J2534 worker gives the functional presence primitive no
/// transmit time to measure it from, so its window is not judged either (`no_transmit_time`).
fn assert_timed(item: &serde_json::Value) {
    assert!(item["response_ms"].is_null(), "{item}");
    assert_eq!(item["clock"], "device", "{item}");
    assert!(item["epoch"].is_number(), "{item}");
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

    /// The presence exchange's entry: the only exchange of the run whose request is TesterPresent.
    /// The ECU phase then reads the example ECU's software version (ADR-401), which is another
    /// exchange.
    fn exchange(&self, what: &str) -> &serde_json::Value {
        let exchanges: Vec<_> = self
            .log
            .iter()
            .filter(|line| line["event"] == "exchange" && line["request"]["sid"] == "0x3e")
            .collect();
        assert_eq!(exchanges.len(), 1, "{what}\n{:#?}", self.log);
        exchanges[0]
    }
}

/// Checks a run whose table the adapter refused at connect: the refused table's size, the sizes
/// of the parts that were linked in turn, and the one answer of the simulated ECU, which marks
/// every endpoint with its response identifier.
fn halved(found: &Found, what: &str, refused: &[usize], parts: &[usize], pairs: &[(u32, u32)]) {
    let report = found
        .run
        .result
        .as_ref()
        .unwrap_or_else(|error| panic!("{what}: {error}\n{:#?}", found.log));
    let sizes: Vec<usize> = report
        .refused_tables()
        .iter()
        .map(|refused| refused.table.len())
        .collect();
    assert_eq!(sizes, refused, "{what}");
    assert_eq!(report.refused_tables()[0].table, pairs, "{what}");
    assert_eq!(
        report.presence_links_opened() as usize,
        parts.len(),
        "{what}"
    );
    let tables: Vec<usize> = found
        .log
        .iter()
        .filter(|line| line["event"] == "link_opened")
        .map(|line| line["functional"]["table"].as_array().map_or(0, Vec::len))
        .collect();
    assert_eq!(tables, parts, "{what}\n{:#?}", found.log);
    assert!(!report.presence_interrupted(), "{what}");
    assert_eq!(
        report
            .presence()
            .iter()
            .map(|answer| (answer.rx_id, answer.bytes.clone()))
            .collect::<Vec<_>>(),
        [(ECU_RESPONSE_ID, vec![0x7E, 0x00])],
        "{what}"
    );
    for endpoint in report.endpoints() {
        assert_eq!(
            report.answered(&endpoint.key),
            endpoint.key.rx_id == ECU_RESPONSE_ID,
            "{what}: {:?}",
            endpoint.key
        );
    }
    assert!(!found.run.guards.link_unconfirmed(), "{what}");
    assert_eq!(found.run.comm_log, CommLogStatus::Complete, "{what}");
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
    let found = run_once(&client, root, control_dir, "answer", None, checked_plan()).await;
    assert_eq!(found.heard("answer"), positive());
    let report = found.report("answer");
    assert!(
        report
            .endpoints()
            .iter()
            .all(|endpoint| report.answered(&endpoint.key))
    );
    // After the presence request the job reads the example ECU's definition (ADR-401): the
    // software version, which the first variant's pattern names, and which the simulated ECU
    // answers with the version that pattern expects.
    let read = &report.ecu_reads()[0];
    assert_eq!(read.definition, "example-ecu");
    assert_eq!(read.requests[0].request, [0x22, 0xF1, 0x89]);
    let RequestOutcome::Decoded(fields) = &read.requests[0].declarations[0].outcome else {
        panic!("{:#?}", read.requests);
    };
    assert_eq!(fields.len(), 1, "{fields:?}");
    assert_eq!(fields[0].value, FieldValue::Text("1.0.0".to_owned()));
    assert_eq!(read.matching.len(), 1);
    assert!(read.answered && read.conclusive && !report.ecu_reads_interrupted());
    let reads: Vec<_> = found
        .log
        .iter()
        .filter(|line| line["event"] == "exchange" && line["request"]["sid"] == "0x22")
        .collect();
    assert_eq!(reads.len(), 1, "{:#?}", found.log);
    assert_eq!(reads[0]["origin"], "identification");
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
    // The link forwards pendings, so the rows may rely on that (ADR-376).
    assert_eq!(exchange["pending_visibility"], "forwarded", "{exchange}");
    // No transmit time reaches the log on this link, and an identification job is given no timing
    // margin (ADR-377): nothing is judged against a limit, so the answer is placed on its clock
    // and `undetermined`, and so is the entry (ADR-400).
    assert_timed(&exchange["answers"][0]);
    assert_eq!(exchange["answers"][0]["classification"], "undetermined");
    assert_eq!(exchange["classification"], "undetermined", "{exchange}");
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
        checked_plan(),
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
    // Never `vehicle`, whatever the link has answered before (ADR-400).
    assert_eq!(
        found.exchange("silent")["classification"],
        "undetermined",
        "{}",
        found.exchange("silent")
    );

    // The run's guards hold the VCI's lock until they are dropped.
    drop(found);

    // A negative answer is presence too, with its code.
    let negative = r#"{"negative_response": {"nrc": "conditions_not_correct"}}"#;
    let found = run_once(
        &client,
        root,
        control_dir,
        "negative",
        Some(negative),
        checked_plan(),
    )
    .await;
    assert_eq!(
        found.heard("negative"),
        [(ECU_RESPONSE_ID, "7f3e22".to_owned())]
    );
    assert_eq!(found.exchange("negative")["outcome"]["result"], "negative");
    let negative_answer = &found.exchange("negative")["answers"][0];
    assert_timed(negative_answer);
    assert_eq!(negative_answer["outcome"]["result"], "negative");
    assert_eq!(negative_answer["classification"], "undetermined");

    // The run's guards hold the VCI's lock until they are dropped.
    drop(found);

    // A chain of pendings, then the final answer: one answer.
    let pending = r#"{"response_pending": {"count": 2, "interval_ms": 300}}"#;
    let found = run_once(
        &client,
        root,
        control_dir,
        "pending",
        Some(pending),
        checked_plan(),
    )
    .await;
    assert_eq!(found.heard("pending"), positive());
    assert_eq!(
        found.exchange("pending")["answers"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    let pending_exchange = found.exchange("pending");
    assert_timed(&pending_exchange["answers"][0]);
    assert_eq!(
        pending_exchange["answers"][0]["classification"], "undetermined",
        "{pending_exchange}"
    );
    assert_eq!(
        pending_exchange["classification"], "undetermined",
        "{pending_exchange}"
    );
    assert_eq!(
        pending_exchange["pending"].as_array().map(Vec::len),
        Some(2)
    );

    // The run's guards hold the VCI's lock until they are dropped.
    drop(found);

    // The ECU answers twice on its identifier, 100 ms apart: two answers, each placed on its
    // clock.
    let repeat = r#"{"repeat_response": {"interval_ms": 100}}"#;
    let found = run_once(
        &client,
        root,
        control_dir,
        "repeat",
        Some(repeat),
        checked_plan(),
    )
    .await;
    let exchange = found.exchange("repeat");
    let answers = exchange["answers"].as_array().expect("answers");
    assert_eq!(answers.len(), 2, "{exchange}");
    assert_eq!(answers[0]["rx_id"], answers[1]["rx_id"], "{exchange}");
    for item in answers {
        assert_timed(item);
        assert_eq!(item["classification"], "undetermined", "{exchange}");
    }
    assert!(
        answers[1]["vci_us"].as_u64() > answers[0]["vci_us"].as_u64(),
        "{exchange}"
    );
    assert_eq!(exchange["classification"], "undetermined", "{exchange}");
    assert!(
        exchange["reason"]
            .as_str()
            .unwrap()
            .contains("2 answers on 1 identifier"),
        "{exchange}"
    );

    // The run's guards hold the VCI's lock until they are dropped.
    drop(found);

    // An answer after the window has ended is not presence.
    let late = format!(r#"{{"delay_response": {{"ms": {LATE_MS}}}}}"#);
    let found = run_once(
        &client,
        root,
        control_dir,
        "late",
        Some(&late),
        checked_plan(),
    )
    .await;
    assert_eq!(found.heard("late"), []);
    assert_eq!(found.exchange("late")["outcome"]["result"], "no_response");

    drop(found);

    // Rows E1 and E2 (ADR-398): a receive table the adapter refuses at connect is halved.
    // Two pairs that answer on one response identifier are refused as not unique: each half
    // gets a link of its own, and the one answer marks both endpoints.
    let shared = [(0x7E0, ECU_RESPONSE_ID), (0x7E1, ECU_RESPONSE_ID)];
    let found = run_once(
        &client,
        root,
        control_dir,
        "not-unique",
        None,
        plan_over(&shared),
    )
    .await;
    halved(&found, "not-unique", &[2], &[1, 1], &shared);
    drop(found);

    // Eleven distinct pairs are over the adapter's limit of ten filters: 6 and 5.
    let mut many = vec![(0x7E0, ECU_RESPONSE_ID)];
    many.extend((1..=10).map(|i| (0x710 + i, 0x790 + i)));
    let found = run_once(
        &client,
        root,
        control_dir,
        "over-limit",
        None,
        plan_over(&many),
    )
    .await;
    halved(&found, "over-limit", &[11], &[6, 5], &many);
    drop(found);

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}

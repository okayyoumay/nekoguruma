// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_PDU_API_ROOT_FILE overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! The part of `hw_t2_t3_read` that needs no hardware (design 13.5, ADR-275), on a D-PDU API VCI:
//! the check's body (`ngr_hw_checks::checks::t2_t3_read`) identifies the example ECU's variant
//! and runs its read procedure through the agent's job runner, on the harness's guarded client for
//! a T3 profile, against the simulated ECU `iso22900-mock` answers from (ADR-299) in
//! `iso22900-service`. The profile's definition is the example ECU's tables with the end-to-end
//! read procedure beside them (`crates/diag-frontend/examples`). Every request the jobs make goes
//! through the guard, and none is refused. `crates/j2534-0404-service/tests/t2_t3_read_check.rs`
//! runs the same body on `sim-vci` behind the J2534 worker.
//!
//! This file holds a single test, so the process-wide variables it sets for the spawned service
//! cannot race with another test.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ngr_hw_checks::checks::t2_t3_read::{convert, run};
use ngr_hw_checks::guard::{GuardedClient, HandleOptions};
use ngr_hw_checks::profile::{ReadDefinition, Setup, parse};
use ngr_hw_checks::report::RunGuard;
use vci_service_interface::{GetModuleIdsRequest, ModuleConnectRequest};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "NGR Mock D-PDU API";

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../diag-frontend/examples")
}

/// A T3 profile on `iso22900-mock` with the example ECU's link (`protocols.csv`, `comparams.csv`),
/// reading the definition in `definition`.
fn profile_text(definition: &Path) -> String {
    format!(
        r#"
class = "T3"

[vci]
api = "iso22900"
library = "{LIBRARY_NAME}"
module = "iso22900-mock"
firmware_version = "0.0.0"
library_version = "0.0.0"

[link]
protocol = "ISO_14229_3_on_ISO_15765_2"
baud_rate = 500000
pins = [6, 14]
request_id = 0x7E0
response_id = 0x7E8
functional_id = 0x7DF
id_bits = 11
addressing = "normal"

[read]
definition = {definition:?}
procedure = "read.star"
"#
    )
}

/// The example ECU's tables and the end-to-end read procedure, copied into `dir`.
fn definition_dir(dir: &Path) {
    std::fs::create_dir_all(dir).expect("definition directory");
    for entry in std::fs::read_dir(examples().join("example-ecu")).expect("the example") {
        let path = entry.expect("an entry").path();
        std::fs::copy(&path, dir.join(path.file_name().expect("a file name")))
            .expect("the table should copy");
    }
    std::fs::copy(
        examples().join("end-to-end/read.star"),
        dir.join("read.star"),
    )
    .expect("the procedure should copy");
}

#[test]
fn the_read_check_identifies_the_variant_and_runs_its_read_procedure_through_the_guard() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let root = temp.path();
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, "").expect("test config file should be writable");
    let library = iso22900_mock::mock_library_path().expect("the mock cdylib should be built");
    let root_file = iso22900_mock::write_root_file(root, LIBRARY_NAME, &library)
        .expect("root description file should be writable");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::set_var("NGR_PDU_API_ROOT_FILE", &root_file);
        std::env::set_var(
            iso22900_mock::ECU_CONFIG_ENV,
            examples().join("end-to-end/ecu.json"),
        );
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
    }
    let definition = root.join("ecu-definition");
    definition_dir(&definition);
    let profile = parse(&profile_text(&definition)).expect("the profile is valid");
    let read: ReadDefinition = profile
        .read
        .clone()
        .expect("the profile names its definition");

    // The jobs' VMs run on blocking threads that block on the runtime for every primitive.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(120), async {
                let worker = WorkerProcess::launch(
                    Path::new(env!("CARGO_BIN_EXE_iso22900-service")),
                    ServiceKind::Iso22900,
                    LIBRARY_NAME,
                    OperatingMode::User,
                    &LaunchOptions {
                        startup_timeout: Duration::from_secs(20),
                        request_timeout: Duration::from_secs(5),
                        ..LaunchOptions::default()
                    },
                )
                .await
                .expect("worker should launch");
                let mut client = worker
                    .connect(&ConnectOptions::default())
                    .await
                    .expect("client should connect");
                // The harness holds the module, as `open_device` leaves it for a hardware run.
                let module = client
                    .get_module_ids(GetModuleIdsRequest {})
                    .await
                    .expect("the worker lists its module")
                    .into_inner()
                    .module_id_list
                    .and_then(|list| list.module_data.into_iter().next())
                    .and_then(|module| module.module_handle)
                    .expect("one module");
                client
                    .module_connect(ModuleConnectRequest {
                        module_handle: Some(module),
                    })
                    .await
                    .expect("the module connects");

                let Setup::Link { vci, .. } = &profile.setup else {
                    unreachable!("a T3 profile has one link")
                };
                let mut guard = RunGuard::start_in(&profile, "hw_t2_t3_read", root.join("reports"));
                let guarded = GuardedClient::attach(
                    &profile,
                    vci,
                    client,
                    module,
                    HandleOptions::default(),
                    &guard,
                )
                .await
                .expect("the guarded client holds the module");
                let documents = convert(&read).expect("the definition converts");
                let outcome = match run(&guarded, &documents, &root.join("work")).await {
                    Ok(outcome) => outcome,
                    Err(error) => panic!(
                        "the check passes: {error}; the guard decided {:#?}",
                        guarded.trace().lock().unwrap()
                    ),
                };
                // The simulated ECU reports software version 1.0.0, the first generation's.
                assert_eq!(outcome.variant, "exampleEcuV1");
                assert!(outcome.state.call_stack.is_empty(), "{:?}", outcome.state);
                // Every request the jobs made passed the guard.
                let trace = guarded.trace();
                let refused: Vec<_> = trace
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, decision)| decision.is_err())
                    .map(|(rpc, decision)| format!("{rpc}: {decision:?}"))
                    .collect();
                assert!(refused.is_empty(), "{refused:#?}");
                drop(guarded);
                guard.pass();
                assert!(guard.run_teardown().is_empty(), "the module disconnects");
                drop(guard);
                worker
                    .stop(Duration::from_secs(5))
                    .await
                    .expect("the worker stops");
            })
            .await
        })
        .expect("the check should finish in time");
}

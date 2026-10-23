// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_J2534_DEFINITION_DIR overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! The part of `hw_t2_t3_read` that needs no hardware (design 13.5, ADR-275): the check's body
//! (`ngr_hw_checks::checks::t2_t3_read`) identifies the example ECU's variant and runs its read
//! procedure through the agent's job runner, on the harness's guarded client for a T3 profile,
//! against the simulated ECU behind `sim-vci` in `j2534-0404-service`. The profile's definition
//! is the example ECU's tables with the end-to-end read procedure beside them
//! (`crates/diag-frontend/examples`). Every request the jobs make goes through the guard, and
//! none is refused.
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

const LIBRARY_NAME: &str = "sim-vci";

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../diag-frontend/examples")
}

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

/// A T3 profile on `sim-vci` with the example ECU's link (`protocols.csv`, `comparams.csv`),
/// reading the definition in `definition`.
fn profile_text(definition: &Path) -> String {
    format!(
        r#"
class = "T3"

[vci]
api = "j2534-0404"
library = "{LIBRARY_NAME}"
module = "sim-vci"
label = "sim"
pname = "sim-vci"
firmware_version = "unchecked here"
library_version = "unchecked here"

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
    let definitions = root.join("definitions");
    std::fs::create_dir_all(&definitions).expect("definition directory");
    let config_path = root.join("sim-vci.toml");
    std::fs::write(&config_path, "").expect("test config file should be writable");
    j2534_0404_mock::write_definition(&definitions, LIBRARY_NAME, &sim_vci_path())
        .expect("definition file should be writable");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &definitions);
        std::env::set_var("NGR_SIM_ECU_CONFIG", examples().join("end-to-end/ecu.json"));
        std::env::remove_var("NGR_SIM_ECU_STATE");
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
                    Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
                    ServiceKind::J2534V0404,
                    LIBRARY_NAME,
                    OperatingMode::User,
                    &LaunchOptions {
                        startup_timeout: Duration::from_secs(20),
                        request_timeout: Duration::from_secs(5),
                        long_size: Some(long_size()),
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

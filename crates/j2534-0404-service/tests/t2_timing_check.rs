// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_J2534_DEFINITION_DIR overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! The part of `hw_t2_timing` that needs no hardware (design 13.5, ADR-275): the check's body
//! (`ngr_hw_checks::checks::t2_timing`) runs on the harness's guarded handle for a T2 profile,
//! through `j2534-0404-service`, against the simulated ECU behind `sim-vci`. The ECU is
//! configured with a P2 and P2* other than `sim-ecu`'s defaults, so the test shows the reported
//! values come from the ECU's answer through the worker and the VCI, and that the comparison with
//! `sim-ecu`'s values marks them as different.
//!
//! This file holds a single test, so the process-wide variables it sets for the spawned service
//! cannot race with another test.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ngr_hw_checks::checks::t2_timing::{SAMPLES, SessionTiming, run};
use ngr_hw_checks::guard::{GuardedHandle, HandleOptions};
use ngr_hw_checks::profile::{Setup, parse};
use ngr_hw_checks::report::RunGuard;
use vci_service_interface::{GetModuleIdsRequest, ModuleConnectRequest};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";

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

/// A T2 profile on `sim-vci` with the example ECU's link.
const PROFILE: &str = r#"
class = "T2"

[vci]
api = "j2534-0404"
library = "sim-vci"
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
"#;

/// The simulated ECU's configuration: the end-to-end example's, with its own P2 and P2*.
fn ecu_config(path: &Path) {
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../diag-frontend/examples/end-to-end/ecu.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(example).expect("the example ECU"))
            .expect("the example ECU is JSON");
    config["p2_server_max_ms"] = 25.into();
    config["p2_star_server_max_ms"] = 2_000.into();
    std::fs::write(path, config.to_string()).expect("the ECU configuration should be writable");
}

#[test]
fn the_timing_check_reads_the_ecu_s_p2_and_p2_star_through_the_guard() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let root = temp.path();
    let definitions = root.join("definitions");
    std::fs::create_dir_all(&definitions).expect("definition directory");
    let config_path = root.join("sim-vci.toml");
    std::fs::write(&config_path, "").expect("test config file should be writable");
    j2534_0404_mock::write_definition(&definitions, LIBRARY_NAME, &sim_vci_path())
        .expect("definition file should be writable");
    let ecu = root.join("ecu.json");
    ecu_config(&ecu);
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &definitions);
        std::env::set_var("NGR_SIM_ECU_CONFIG", &ecu);
        std::env::remove_var("NGR_SIM_ECU_STATE");
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
    }
    let profile = parse(PROFILE).expect("the profile is valid");

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
                    unreachable!("a T2 profile has one link")
                };
                let mut guard = RunGuard::start_in(&profile, "hw_t2_timing", root.join("reports"));
                let mut handle = GuardedHandle::attach(
                    &profile,
                    vci,
                    client,
                    module,
                    HandleOptions::default(),
                    &guard,
                )
                .await
                .expect("the handle opens the profile's link");
                let [cll] = *handle.links() else {
                    panic!("one link, not {:?}", handle.links());
                };
                let outcome = run(&mut handle, cll).await.expect("the check passes");
                assert_eq!(
                    outcome.reported,
                    SessionTiming {
                        p2_ms: 25,
                        p2_star_ms: 2_000
                    }
                );
                assert_eq!(outcome.simulator, SessionTiming::simulator());
                assert!(!outcome.matches_simulator());
                assert_eq!(outcome.round_trips.len(), SAMPLES);
                assert!(
                    outcome
                        .summary()
                        .contains("P2 25 ms (sim-ecu 50 ms, differs)"),
                    "{}",
                    outcome.summary()
                );
                drop(handle);
                guard.pass();
                assert!(guard.run_teardown().is_empty(), "the link and module close");
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

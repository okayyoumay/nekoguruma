// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_PDU_API_ROOT_FILE overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! The part of `hw_t2_timing` that needs no hardware (design 13.5, ADR-275), on a D-PDU API VCI:
//! the check's body (`ngr_hw_checks::checks::t2_timing`) runs on the harness's guarded handle for
//! a T2 profile, through `iso22900-service`, against the simulated ECU `iso22900-mock` answers
//! from (ADR-299). The ECU is configured with a P2 and P2* other than `sim-ecu`'s defaults, so
//! the test shows the reported values come from the ECU's answer, and that the comparison with
//! `sim-ecu`'s values marks them as different. The mock gives each response item its request's
//! transmit done and its own start of message on the module's clock (ISO 22900-2 (2022)
//! 10.1.4.11.4), so each latency is the VCI's, measured from the transmit done to the start of
//! the response (`vci_tx_done_to_rx_start`) and judged against the reported P2 beyond the
//! profile's margin (ADR-356). The ECU answers at once, so the latency is the mock's minimum.
//! `crates/j2534-0404-service/tests/t2_timing_check.rs` runs the same body on `sim-vci` behind
//! the J2534 worker, which measures to the response's end.
//!
//! This file holds a single test, so the process-wide variables it sets for the spawned service
//! cannot race with another test.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ngr_hw_checks::checks::t2_timing::{Judgement, MeasuredBy, SAMPLES, SessionTiming, run};
use ngr_hw_checks::guard::{GuardedHandle, HandleOptions};
use ngr_hw_checks::profile::{Setup, parse};
use ngr_hw_checks::report::RunGuard;
use vci_service_interface::{GetModuleIdsRequest, ModuleConnectRequest};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "NGR Mock D-PDU API";

/// A T2 profile on `iso22900-mock` with the example ECU's link.
const PROFILE: &str = r#"
class = "T2"

[vci]
api = "iso22900"
library = "NGR Mock D-PDU API"
module = "iso22900-mock"
firmware_version = "0.0.0"
library_version = "0.0.0"
timing_margin_us = 500

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
fn the_timing_check_measures_each_latency_from_the_transmit_done_through_the_guard() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let root = temp.path();
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, "").expect("test config file should be writable");
    let library = iso22900_mock::mock_library_path().expect("the mock cdylib should be built");
    let root_file = iso22900_mock::write_root_file(root, LIBRARY_NAME, &library)
        .expect("root description file should be writable");
    let ecu = root.join("ecu.json");
    ecu_config(&ecu);
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::set_var("NGR_PDU_API_ROOT_FILE", &root_file);
        std::env::set_var(iso22900_mock::ECU_CONFIG_ENV, &ecu);
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
                assert_eq!(outcome.samples.len(), SAMPLES);
                assert_eq!(outcome.margin, Some(Duration::from_micros(500)));
                for sample in &outcome.samples {
                    // The mock stamps the request's transmit done and the response's start on
                    // its item, so the latency is measured to the start of the response.
                    let (latency, by) = sample.latency.unwrap_or_else(|| {
                        panic!("the VCI measured {}: {sample:?}", sample.request)
                    });
                    assert_eq!(by, MeasuredBy::VciTxDoneToRxStart, "{sample:?}");
                    assert!(latency > Duration::ZERO, "{sample:?}");
                    assert!(latency < Duration::from_millis(25), "{sample:?}");
                    assert_eq!(sample.judgement, Judgement::WithinP2, "{sample:?}");
                }
                let summary = outcome.summary();
                assert!(
                    summary.contains("P2 25 ms (sim-ecu 50 ms, differs)"),
                    "{summary}"
                );
                assert!(
                    summary.contains("(vci_tx_done_to_rx_start), within P2"),
                    "{summary}"
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

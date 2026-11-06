// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_PDU_API_ROOT_FILE overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! The guarded handle's indication path (`note_indication`, ADR-356) on a D-PDU API VCI: the
//! handle's `send_receive_timed` runs on a T2 profile through `iso22900-service` against the
//! simulated ECU `iso22900-mock` answers from (ADR-299), with the mock's response items carrying
//! no times in their `TimestampFlags` (`SimEcuCommand::ResponseTimestamps`). The handle then has
//! only the transmit and start-of-message indication items its link asked for with
//! `CP_TransmitIndEnable` and `CP_StartMsgIndEnable` (ISO 22900-2 (2022) B.5.1) to take the
//! transmit done and the response's start from. The VIN read has a response of several frames, so
//! it is announced by a start-of-message indication; a session control has a single-frame
//! response, which is not (B.5.1, D.2.2), so the handle has no start for it.
//! `t2_timing_check.rs` covers the times in the response's own item.
//!
//! This file holds a single test, so the process-wide variables it sets for the spawned service
//! cannot race with another test.

use std::path::Path;
use std::time::Duration;

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

#[test]
fn the_handle_takes_its_times_from_the_indication_items() {
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
        std::env::remove_var(iso22900_mock::ECU_CONFIG_ENV);
        // The default ECU, whose responses carry no times of their own.
        std::env::set_var(
            iso22900_mock::ECU_COMMANDS_ENV,
            r#"[{"attach":{}},{"response_timestamps":{"enabled":false}}]"#,
        );
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

                // A multi-frame response: both times come from the indications, and the start is
                // the response's own time, as the mock stamps it.
                let read = handle
                    .send_receive_timed(cll, &[0x22, 0xF1, 0x90])
                    .await
                    .expect("the VIN read is answered");
                assert_eq!(read.response.len(), 3 + 17);
                let times = read.times;
                let (tx_done, rx_start, rx_end) = (
                    times
                        .tx_done
                        .expect("the transmit indication gave the transmit done"),
                    times.rx_start.expect("the start indication gave the start"),
                    times.rx_end.expect("the response's item gave the end"),
                );
                assert!(tx_done.clock.is_some(), "{times:?}");
                assert_eq!(
                    tx_done.until(rx_start).map(|us| us > 0),
                    Some(true),
                    "{times:?}"
                );
                assert_eq!(rx_start.until(rx_end), Some(0), "{times:?}");

                // A single-frame response has no start-of-message indication, so the handle has
                // no start for it; the transmit done still comes from the indication.
                let session = handle
                    .send_receive_timed(cll, &[0x10, 0x01])
                    .await
                    .expect("the session control is answered");
                assert_eq!(session.response[0], 0x50);
                let times = session.times;
                assert!(times.tx_done.is_some(), "{times:?}");
                assert_eq!(times.rx_start, None, "{times:?}");
                let rx_end = times.rx_end.expect("the response's item gave the end");
                assert_eq!(
                    times.tx_done.unwrap().until(rx_end).map(|us| us > 0),
                    Some(true),
                    "{times:?}"
                );
                // The second exchange's transmit done is its own, not the first one's.
                assert_eq!(
                    tx_done.until(times.tx_done.unwrap()).map(|us| us > 0),
                    Some(true)
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
        .expect("the test should finish in time");
}

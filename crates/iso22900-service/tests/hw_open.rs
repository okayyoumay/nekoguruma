// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_PDU_API_ROOT_FILE overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! The real-hardware harness opens a D-PDU API device through `iso22900-service` and refuses a
//! device that is not the profile's before it creates a link (design 13.5, ADR-275 item 4).
//! `ngr_hw_checks::open::open_device` starts the worker for `iso22900-mock`, matches the module
//! and the versions the mock reports with the profile, and creates no link in any case: a
//! refusal is returned from the match, with the worker stopped.
//!
//! The mock reports the module `iso22900-mock`, and its firmware and library versions code as
//! `0.0.0`.

use std::path::Path;

use ngr_hw_checks::device::MatchRefusal;
use ngr_hw_checks::open::{OpenError, open_device};
use ngr_hw_checks::profile::{Api, Vci};
use serial_test::serial;
use worker_host::service::{LaunchOptions, OperatingMode, Stopped, WorkerLayout};

const LIBRARY_NAME: &str = "NGR Mock D-PDU API";

/// Installs the service binary as `<workers>/<ABI name>/iso22900-service`.
fn install_worker(workers: &Path, abi_name: &str) {
    let service = Path::new(env!("CARGO_BIN_EXE_iso22900-service"));
    let dir = workers.join(abi_name);
    std::fs::create_dir_all(&dir).expect("workers directory should be writable");
    let target = dir.join(service.file_name().expect("service binary has a file name"));
    if std::fs::hard_link(service, &target).is_err() {
        std::fs::copy(service, &target).expect("service binary should be copyable");
    }
}

/// The profile's VCI: the mock, with `module` and the versions the profile records.
fn vci(module: &str, firmware: &str, library: &str) -> Vci {
    Vci {
        api: Api::Iso22900,
        library: LIBRARY_NAME.to_owned(),
        module: module.to_owned(),
        label: None,
        pname: None,
        firmware_version: firmware.to_owned(),
        library_version: library.to_owned(),
        timing_margin_us: None,
    }
}

/// Opens `vci` against the mock; `Ok` holds the device already closed, with how its worker ended.
async fn open(vci: &Vci) -> Result<Stopped, OpenError> {
    let temp = tempfile::tempdir().expect("temporary directory");
    let root = temp.path();
    let library = iso22900_mock::mock_library_path().expect("the mock cdylib should be built");
    let abi = worker_host::abi::detect_file(&library).expect("the mock should have a known ABI");
    let workers = WorkerLayout {
        root: root.join("workers"),
    };
    install_worker(&workers.root, abi.name());
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, "").expect("test config file should be writable");
    let root_file = iso22900_mock::write_root_file(root, LIBRARY_NAME, &library)
        .expect("root description file should be writable");
    // SAFETY: the tests of this file are `#[serial]`, so none reads the environment meanwhile.
    unsafe {
        std::env::set_var("NGR_PDU_API_ROOT_FILE", &root_file);
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
    }
    let device = open_device(
        vci,
        &workers,
        OperatingMode::User,
        &LaunchOptions::default(),
    )
    .await?;
    assert_eq!(device.abi(), abi);
    assert_eq!(device.module().name, "iso22900-mock");
    assert_eq!(device.version().hw_name, "Mock Hardware");
    device.close().await
}

fn refusal(result: Result<Stopped, OpenError>) -> MatchRefusal {
    match result {
        // The worker was stopped and exited by itself, not killed, so the cleanup ran.
        Err(OpenError::Refused {
            refusal,
            stopped: Some(Stopped::Exited),
        }) => refusal,
        other => panic!("the device should be refused, not {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_matching_d_pdu_device_opens_and_closes() {
    let stopped = open(&vci("iso22900-mock", "0.0.0", "0.0.0"))
        .await
        .expect("the mock matches the profile");
    assert_eq!(stopped, Stopped::Exited);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_module_the_worker_does_not_report_is_refused() {
    let result = open(&vci("another module", "0.0.0", "0.0.0")).await;
    assert_eq!(
        refusal(result),
        MatchRefusal::NoModule {
            module: "another module".to_owned()
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_different_firmware_version_is_refused_after_opening() {
    let result = open(&vci("iso22900-mock", "1.2.3", "0.0.0")).await;
    assert_eq!(
        refusal(result),
        MatchRefusal::FirmwareDiffers {
            reported: "0.0.0".to_owned(),
            profile: "1.2.3".to_owned()
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_different_library_version_is_refused_after_opening() {
    let result = open(&vci("iso22900-mock", "0.0.0", "1.2.3")).await;
    assert_eq!(
        refusal(result),
        MatchRefusal::LibraryVersionDiffers {
            reported: "0.0.0".to_owned(),
            profile: "1.2.3".to_owned()
        }
    );
}

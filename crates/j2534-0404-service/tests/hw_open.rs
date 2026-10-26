// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_J2534_DEFINITION_DIR overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! The real-hardware harness opens a J2534 device through `j2534-0404-service` and refuses a
//! device that is not the profile's before it creates a link (design 13.5, ADR-275 item 4).
//! `ngr_hw_checks::open::open_device` starts the worker for `j2534-0404-mock`, reads the
//! `modules` entries of the configuration the worker reads, matches the module the worker
//! reports and the versions the mock reports (`1.0.0` for the firmware and the library) with the
//! profile, and creates no link in any case: a refusal is returned from the match, with the
//! worker stopped.
//!
//! A module's label and library are the worker's own answers (the configured label, and the
//! library name it loaded), so `NoModule`, `SeveralModules` and `LibraryDiffers` cannot arise
//! from a worker that reports one module per configured entry and the profile's library; the
//! match's unit tests cover them.

use std::path::Path;

use ngr_hw_checks::device::MatchRefusal;
use ngr_hw_checks::guard::{GuardRefusal, GuardedHandle, HandleError, HandleOptions};
use ngr_hw_checks::open::{OpenError, open_device};
use ngr_hw_checks::profile::{Api, Vci};
use ngr_hw_checks::report::RunGuard;
use serial_test::serial;
use worker_host::service::{LaunchOptions, OperatingMode, Stopped, WorkerLayout};

const LIBRARY_NAME: &str = "mock-lib";

/// Installs the service binary as `<workers>/<ABI name>/j2534-0404-service`.
fn install_worker(workers: &Path, abi_name: &str) {
    let service = Path::new(env!("CARGO_BIN_EXE_j2534-0404-service"));
    let dir = workers.join(abi_name);
    std::fs::create_dir_all(&dir).expect("workers directory should be writable");
    let target = dir.join(service.file_name().expect("service binary has a file name"));
    if std::fs::hard_link(service, &target).is_err() {
        std::fs::copy(service, &target).expect("service binary should be copyable");
    }
}

/// The profile's VCI: the mock, with the configured entry `label` and `pname` and the versions
/// the profile records.
fn vci(label: &str, pname: &str, firmware: &str, library: &str) -> Vci {
    Vci {
        api: Api::J2534V0404,
        library: LIBRARY_NAME.to_owned(),
        module: "description only".to_owned(),
        label: Some(label.to_owned()),
        pname: Some(pname.to_owned()),
        firmware_version: firmware.to_owned(),
        library_version: library.to_owned(),
        timing_margin_us: None,
        drift_bound_ppm: None,
        clock_margin_us: None,
    }
}

/// A worker layout and configuration for the mock with `modules` configured as `(label, pname)`
/// entries, in a temporary directory that lives as long as the value.
struct Environment {
    _temp: tempfile::TempDir,
    workers: WorkerLayout,
    abi: worker_host::abi::Abi,
}

fn environment(modules: &[(&str, &str)]) -> Environment {
    let temp = tempfile::tempdir().expect("temporary directory");
    let root = temp.path();
    let library = j2534_0404_mock::mock_library_path().expect("the mock cdylib should be built");
    let abi = worker_host::abi::detect_file(&library).expect("the mock should have a known ABI");
    let workers = WorkerLayout {
        root: root.join("workers"),
    };
    install_worker(&workers.root, abi.name());
    let definitions = root.join("definitions");
    std::fs::create_dir_all(&definitions).expect("definition directory");
    j2534_0404_mock::write_definition(&definitions, LIBRARY_NAME, &library)
        .expect("definition file should be writable");
    let mut config = String::new();
    for (label, pname) in modules {
        config.push_str(&format!(
            "[[config.apis.j2534-0404.libs.{LIBRARY_NAME}.modules]]\n\
             label = \"{label}\"\npname = \"{pname}\"\n"
        ));
    }
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, config).expect("test config file should be writable");
    // SAFETY: the tests of this file are `#[serial]`, so none reads the environment meanwhile.
    unsafe {
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &definitions);
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
    }
    Environment {
        _temp: temp,
        workers,
        abi,
    }
}

/// Opens `vci` against the mock with `modules` configured as `(label, pname)` entries; `Ok` holds
/// the device already closed, with how its worker ended.
async fn open(vci: &Vci, modules: &[(&str, &str)]) -> Result<Stopped, OpenError> {
    let env = environment(modules);
    let device = open_device(
        vci,
        &env.workers,
        OperatingMode::User,
        &LaunchOptions::default(),
    )
    .await?;
    assert_eq!(device.abi(), env.abi);
    // What the launch resolved reaches the opened device: the library of the definition the
    // lookup found, and the width the worker was started with (no `LongSize` declared here, so
    // the ABI's default).
    let library = j2534_0404_mock::mock_library_path().expect("the mock cdylib");
    assert_eq!(device.library().file_name(), library.file_name());
    assert_eq!(device.long_size(), env.abi.default_long_size());
    assert_eq!(device.module().name, "Bench 1");
    assert_eq!(device.module().additional_info, LIBRARY_NAME);
    assert_eq!(device.version().hw_name, "1.0.0");
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

const ENTRY: (&str, &str) = ("Bench 1", "J2534-2:mock");

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_matching_j2534_device_opens_and_closes() {
    let stopped = open(&vci("Bench 1", "J2534-2:mock", "1.0.0", "1.0.0"), &[ENTRY])
        .await
        .expect("the mock matches the profile");
    assert_eq!(stopped, Stopped::Exited);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_label_that_is_not_configured_is_refused() {
    let result = open(&vci("Bench 2", "J2534-2:mock", "1.0.0", "1.0.0"), &[ENTRY]).await;
    assert_eq!(
        refusal(result),
        MatchRefusal::UnknownLabel {
            label: "Bench 2".to_owned()
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn several_configured_modules_are_refused() {
    let result = open(
        &vci("Bench 1", "J2534-2:mock", "1.0.0", "1.0.0"),
        &[ENTRY, ("Bench 2", "J2534-2:other")],
    )
    .await;
    assert_eq!(
        refusal(result),
        MatchRefusal::SeveralConfigured { count: 2 }
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_different_device_name_is_refused() {
    let result = open(&vci("Bench 1", "J2534-2:other", "1.0.0", "1.0.0"), &[ENTRY]).await;
    assert_eq!(
        refusal(result),
        MatchRefusal::PnameDiffers {
            label: "Bench 1".to_owned(),
            configured: "J2534-2:mock".to_owned(),
            profile: "J2534-2:other".to_owned()
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_different_firmware_version_is_refused_after_opening() {
    let result = open(&vci("Bench 1", "J2534-2:mock", "2.0.0", "1.0.0"), &[ENTRY]).await;
    assert_eq!(
        refusal(result),
        MatchRefusal::FirmwareDiffers {
            reported: "1.0.0".to_owned(),
            profile: "2.0.0".to_owned()
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_different_library_version_is_refused_after_opening() {
    let result = open(&vci("Bench 1", "J2534-2:mock", "1.0.0", "2.0.0"), &[ENTRY]).await;
    assert_eq!(
        refusal(result),
        MatchRefusal::LibraryVersionDiffers {
            reported: "1.0.0".to_owned(),
            profile: "2.0.0".to_owned()
        }
    );
}

/// The guarded handle on a device the harness opened through a worker process: the profile's link
/// is created and connected through the guard, a request reaches the worker (the mock answers
/// no request), a state-changing request is refused before it does, and the teardown closes the
/// link and the device, so the worker exits by itself.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_handle_opens_a_t3_profile_on_an_opened_device_and_closes_it() {
    let env = environment(&[ENTRY]);
    let profile = ngr_hw_checks::profile::parse(
        "class = \"T3\"\n[vci]\napi = \"j2534-0404\"\nlibrary = \"mock-lib\"\n\
         module = \"description only\"\nlabel = \"Bench 1\"\npname = \"J2534-2:mock\"\n\
         firmware_version = \"1.0.0\"\nlibrary_version = \"1.0.0\"\n\
         [link]\nprotocol = \"ISO_14229_3_on_ISO_15765_2\"\nbaud_rate = 500000\n\
         pins = [6, 14]\nrequest_id = 0x7E0\nresponse_id = 0x7E8\nid_bits = 11\n\
         addressing = \"normal\"\n",
    )
    .expect("a T3 profile");
    let ngr_hw_checks::profile::Setup::Link { vci, .. } = &profile.setup else {
        panic!("a one-link profile");
    };
    let device = open_device(
        vci,
        &env.workers,
        OperatingMode::User,
        &LaunchOptions::default(),
    )
    .await
    .expect("the mock matches the profile");

    let reports = tempfile::tempdir().expect("temporary directory");
    let mut run = RunGuard::start_in(&profile, "hw_open", reports.path().join("reports"));
    let mut handle = GuardedHandle::open(
        &profile,
        vci,
        device,
        HandleOptions {
            request_timeout: std::time::Duration::from_secs(5),
            response_timeout: Some(std::time::Duration::from_secs(1)),
        },
        &run,
    )
    .await
    .expect("the handle creates and connects the profile's link");
    let [cll] = *handle.links() else {
        panic!("one link");
    };

    // The read reaches the worker; the mock has no ECU behind it.
    let silent = handle.send_receive(cll, &[0x22, 0xF1, 0x90]).await;
    assert_eq!(silent, Err(HandleError::NoResponse));
    let refused = handle.send_receive(cll, &[0x2E, 0xF1, 0x90, 0x00]).await;
    assert!(
        matches!(
            refused,
            Err(HandleError::Refused {
                refusal: GuardRefusal::Request { sid: 0x2E, .. },
                ..
            })
        ),
        "{refused:?}"
    );

    drop(handle);
    run.pass();
    // The link and the device are closed, and the worker has exited.
    assert!(run.run_teardown().is_empty());
}

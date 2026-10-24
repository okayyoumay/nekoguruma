//! `hw_t2_timing` (design 13.5, ADR-275): on the T2 profile `NGR_HW_PROFILE` names, a bench ECU on
//! a J2534 VCI, the check reads the P2 and P2* the ECU reports and times a few requests,
//! compared with the values `sim-ecu` uses (`ngr_hw_checks::checks::t2_timing`), through
//! `j2534-0404-service` and the vendor's library on the harness's guarded handle (ADR-334). It sends
//! DiagnosticSessionControl for the default session and TesterPresent only, both on the guard's
//! read-only list.
//!
//! The test is ignored: it needs the attached hardware, a hardware profile and the vendor's
//! registration (design 7.1.1), and CI never runs it. It does not use the debug-only `NGR_J2534_DEFINITION_DIR` override,
//! which would replace the OS lookup of the vendor's library (ADR-282): it fails if that variable
//! is set.
//!
//! The worker is found under `NGR_HW_WORKERS` (the root of a worker directory, `<abi>/<binary>`,
//! design 7.3), or else the host-ABI build of this workspace is installed in a temporary one.
//! The part of the check that needs no hardware runs in CI
//! (`crates/j2534-0404-service/tests/t2_timing_check.rs`).

use std::path::{Path, PathBuf};

use ngr_hw_checks::checks::t2_timing::run;
use ngr_hw_checks::guard::{GuardedHandle, HandleOptions};
use ngr_hw_checks::open::open_device;
use ngr_hw_checks::profile::{Api, Setup};
use ngr_hw_checks::report::{Masker, Outcome, RunGuard};
use ngr_hw_checks::{T2_TIMING_CHECK, begin};
use worker_host::abi::detect_file;
use worker_host::service::{LaunchOptions, OperatingMode, WorkerLayout};

/// The worker directory: `NGR_HW_WORKERS`, else this workspace's host-ABI worker installed under
/// `temp`.
fn workers(temp: &Path) -> WorkerLayout {
    if let Some(root) = std::env::var_os("NGR_HW_WORKERS") {
        return WorkerLayout {
            root: PathBuf::from(root),
        };
    }
    let service = Path::new(env!("CARGO_BIN_EXE_j2534-0404-service"));
    let abi = detect_file(&std::env::current_exe().expect("test executable"))
        .expect("the test executable has a known ABI");
    let dir = temp.join("workers").join(abi.name());
    std::fs::create_dir_all(&dir).expect("workers directory should be writable");
    let target = dir.join(service.file_name().expect("service binary has a file name"));
    if std::fs::hard_link(service, &target).is_err() {
        std::fs::copy(service, &target).expect("service binary should be copyable");
    }
    WorkerLayout {
        root: temp.join("workers"),
    }
}

/// Records `outcome` with `why` and fails the test.
fn stop(guard: &mut RunGuard, outcome: Outcome, why: &str) -> ! {
    guard.finish(outcome, why);
    panic!("{why}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real-hardware check, design 13.5: run with `cargo nextest run --profile hardware --run-ignored=only`"]
async fn hw_t2_timing() {
    // Declared before the run guard, so it is dropped after the guard's teardown: the worker
    // (installed in it) is stopped by then. It is created after `begin`, which comes first.
    let temp: tempfile::TempDir;
    let (profile, mut guard) = begin(&T2_TIMING_CHECK);
    temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => stop(
            &mut guard,
            Outcome::Failed,
            &format!("the temporary directory: {error}"),
        ),
    };
    if std::env::var_os("NGR_J2534_DEFINITION_DIR").is_some() {
        stop(
            &mut guard,
            Outcome::Refused,
            "NGR_J2534_DEFINITION_DIR replaces the OS lookup of the vendor's library (ADR-282); unset it \
             for a hardware run",
        );
    }
    let Setup::Link { vci, .. } = &profile.setup else {
        stop(&mut guard, Outcome::Refused, "the profile has no link");
    };
    if vci.api != Api::J2534V0404 {
        stop(
            &mut guard,
            Outcome::Refused,
            "the profile's VCI is a D-PDU API one; this test drives a J2534 VCI",
        );
    }
    let masker = Masker::for_profile(&profile);
    let device = match open_device(
        vci,
        &workers(temp.path()),
        OperatingMode::User,
        &LaunchOptions::default(),
    )
    .await
    {
        Ok(device) => device,
        Err(error) => {
            // Without NGR_HW_WORKERS only the build for the host's ABI is installed, and a
            // library of another ABI (a 32-bit DLL on Windows) needs its own worker.
            let why = format!(
                "the profile's device {:?} did not open: {error}. If no worker is installed for \
                 the library's ABI, set NGR_HW_WORKERS to a worker directory that holds every \
                 ABI's build (design 7.3)",
                vci.library
            );
            stop(&mut guard, Outcome::Failed, &masker.text(&why));
        }
    };
    // The handle takes the device, so it goes into the report first (design 13.5 "Reports").
    guard
        .report()
        .add_device(vci, device.abi(), device.module(), device.version());
    let mut handle =
        match GuardedHandle::open(&profile, vci, device, HandleOptions::default(), &guard).await {
            Ok(handle) => handle,
            Err(error) => {
                let why = masker.text(&format!(
                    "the link on the profile's device {:?} did not open: {error}",
                    vci.library
                ));
                stop(&mut guard, Outcome::Failed, &why);
            }
        };
    let [cll] = *handle.links() else {
        stop(
            &mut guard,
            Outcome::Failed,
            "the profile's link did not open as one link",
        );
    };
    let result = run(&mut handle, cll).await;
    drop(handle);
    match result {
        // A value that differs from sim-ecu's is recorded in the message, not a failure.
        Ok(outcome) => guard.finish(Outcome::Passed, &outcome.summary()),
        Err(error) => {
            // The report keeps the outcome; the test still fails, so the run command does too.
            let why = masker.text(&error.to_string());
            stop(&mut guard, Outcome::Failed, &why);
        }
    }
}

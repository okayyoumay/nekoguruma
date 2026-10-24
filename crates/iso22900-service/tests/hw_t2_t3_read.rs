//! `hw_t2_t3_read` (design 13.5, ADR-275) on a D-PDU API VCI: on the T2 or T3 profile
//! `NGR_HW_PROFILE` names, a bench ECU or a vehicle on a D-PDU API VCI, the profile's definition
//! (`[read]`) identifies the ECU's variant and runs its read procedure through the agent's job
//! runner on the harness's guarded client (ADR-334), through `iso22900-service` and the vendor's
//! library. The guard refuses any write, routine, reset or security access, so the check sends
//! reads only. `crates/j2534-0404-service/tests/hw_t2_t3_read.rs` is the same check on a J2534 VCI.
//!
//! The test is ignored: it needs the attached hardware, a hardware profile and the vendor's
//! installation (the root description file, design 7.1), and CI never runs it. It does not use
//! the debug-only `NGR_PDU_API_ROOT_FILE` override, which would replace the OS lookup of the
//! vendor's library (ADR-282): it fails if that variable is set.
//!
//! The worker is found under `NGR_HW_WORKERS` (the root of a worker directory, `<abi>/<binary>`,
//! design 7.3), or else the host-ABI build of this workspace is installed in a temporary one.
//! The part of the check that needs no hardware runs in CI (`tests/t2_t3_read_check.rs`).

use std::path::{Path, PathBuf};

use ngr_hw_checks::checks::t2_t3_read::{convert, run};
use ngr_hw_checks::guard::{GuardedClient, HandleOptions};
use ngr_hw_checks::open::open_device;
use ngr_hw_checks::profile::{Api, Setup};
use ngr_hw_checks::report::{Masker, Outcome, RunGuard};
use ngr_hw_checks::{T2_T3_READ_CHECK, begin};
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
    let service = Path::new(env!("CARGO_BIN_EXE_iso22900-service"));
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
async fn hw_t2_t3_read() {
    // Declared before the run guard, so it is dropped after the guard's teardown: the worker
    // (installed in it) is stopped by then. It is created after `begin`, which comes first.
    let temp: tempfile::TempDir;
    let (profile, mut guard) = begin(&T2_T3_READ_CHECK);
    temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => stop(
            &mut guard,
            Outcome::Failed,
            &format!("the temporary directory: {error}"),
        ),
    };
    if std::env::var_os("NGR_PDU_API_ROOT_FILE").is_some() {
        stop(
            &mut guard,
            Outcome::Refused,
            "NGR_PDU_API_ROOT_FILE replaces the OS lookup of the vendor's library (design \
             7.1, ADR-282); unset it for a hardware run",
        );
    }
    let Setup::Link { vci, .. } = &profile.setup else {
        stop(&mut guard, Outcome::Refused, "the profile has no link");
    };
    if vci.api != Api::Iso22900 {
        stop(
            &mut guard,
            Outcome::Refused,
            "the profile's VCI is a J2534 one; this test drives a D-PDU API VCI",
        );
    }
    let Some(read) = profile.read.clone() else {
        stop(
            &mut guard,
            Outcome::Refused,
            "the profile names no definition to read (`[read]`)",
        );
    };
    // Converted before any device opens, so a definition that does not convert sends nothing.
    let documents = match convert(&read) {
        Ok(documents) => documents,
        Err(error) => stop(&mut guard, error_outcome(&error), &error.to_string()),
    };
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
    // The client takes the device, so it goes into the report first (design 13.5 "Reports").
    guard
        .report()
        .add_device(vci, device.abi(), device.module(), device.version());
    let client =
        match GuardedClient::open(&profile, vci, device, HandleOptions::default(), &guard).await {
            Ok(client) => client,
            Err(error) => {
                let why = masker.text(&format!(
                    "the profile's device {:?} could not be held: {error}",
                    vci.library
                ));
                stop(&mut guard, Outcome::Failed, &why);
            }
        };
    let result = run(&client, &documents, &temp.path().join("work")).await;
    drop(client);
    match result {
        Ok(outcome) => guard.finish(
            Outcome::Passed,
            &format!(
                "the ECU is variant {}; its read procedure finished",
                outcome.variant
            ),
        ),
        Err(error) => {
            // The report keeps the outcome; the test still fails, so the run command does too.
            let why = masker.text(&error.to_string());
            stop(&mut guard, error_outcome(&error), &why);
        }
    }
}

/// A check that refused to start is recorded as refused, one that ran and failed as failed.
fn error_outcome(error: &ngr_hw_checks::checks::t2_t3_read::CheckError) -> Outcome {
    match error {
        ngr_hw_checks::checks::t2_t3_read::CheckError::Refused(_) => Outcome::Refused,
        _ => Outcome::Failed,
    }
}

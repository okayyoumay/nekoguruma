//! `hw_t0_two_endpoint_loopback` (design 13.5, ADR-275): on the T0 profile `NGR_HW_PROFILE` names, a
//! loopback of two VCI modules, each end sends single RawMode frames that the other must receive
//! exactly, in both directions, through `j2534-0404-service` and the vendor's library. The result
//! records the library, ABI and `unsigned long` width the worker ran with.
//!
//! The test is ignored: it needs the attached hardware, a hardware profile and the vendor's
//! registration (design 7.1.1), and CI never runs it. It does not use the debug-only
//! `NGR_J2534_DEFINITION_DIR` override, which would replace the OS lookup the check is there to
//! exercise (ADR-282): it fails if that variable is set.
//!
//! The worker is found under `NGR_HW_WORKERS` (the root of a worker directory, `<abi>/<binary>`,
//! design 7.3), or else the host-ABI build of this workspace is installed in a temporary one.
//! The part of the check that needs no hardware runs in CI (`tests/grpc_mock/t0_loopback_check.rs`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use ngr_hw_checks::checks::t0_loopback::{DeviceFacts, Ends, FRAME_TIMEOUT, run};
use ngr_hw_checks::guard::{GuardedHandle, HandleOptions};
use ngr_hw_checks::open::open_device;
use ngr_hw_checks::profile::{Api, Profile, Setup, Vci};
use ngr_hw_checks::report::{Masker, Outcome};
use ngr_hw_checks::{T0_LOOPBACK_CHECK, begin};
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

/// The `LongSize` the library's registration definition declares, the way the launch looked it up.
#[cfg(not(windows))]
fn declared_long_size(vci: &Vci) -> Option<u8> {
    ngr_library_resolver::j2534_0404::resolve(&vci.library)
        .ok()
        .and_then(|resolved| resolved.definition.long_size)
}

/// On Windows the definition comes from the registry views the launch tries; the width is then
/// checked against the ABI default only.
#[cfg(windows)]
fn declared_long_size(_: &Vci) -> Option<u8> {
    None
}

/// The two VCI ends of a T0 profile, or why the check does not run on it.
fn vci_ends(profile: &Profile) -> Result<[Vci; 2], String> {
    let Setup::Loopback(ends) = &profile.setup else {
        return Err("the profile is not a T0 loopback".to_owned());
    };
    let mut found = Vec::new();
    for (at, end) in ends.iter().enumerate() {
        match &end.vci {
            Some(vci) if vci.api == Api::J2534V0404 => found.push(vci.clone()),
            Some(_) => {
                return Err(format!(
                    "endpoint {at} is a D-PDU API end; the check drives J2534 ends"
                ));
            }
            None => {
                return Err(format!(
                    "endpoint {at} is a SocketCAN end ({}); the check drives VCI ends only",
                    end.socketcan.as_deref().unwrap_or("?")
                ));
            }
        }
    }
    let [first, second]: [Vci; 2] = found.try_into().expect("a loopback has two ends");
    if first.identifies_same_module(&second) {
        return Err(
            "both ends are channels of one module; the guarded handle creates no additional-channel \
             link yet, so the check runs on two modules"
                .to_owned(),
        );
    }
    Ok([first, second])
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real-hardware check, design 13.5: run with `cargo nextest run --profile hardware --run-ignored=only`"]
async fn hw_t0_two_endpoint_loopback() {
    // Declared before the run guard, so it is dropped after the guard's teardown: the workers
    // (installed in it) are stopped by then.
    let temp = tempfile::tempdir().expect("temporary directory");
    let (profile, mut guard) = begin(&T0_LOOPBACK_CHECK);
    if std::env::var_os("NGR_J2534_DEFINITION_DIR").is_some() {
        let why = "NGR_J2534_DEFINITION_DIR replaces the OS lookup of the vendor's library \
                   (design 7.1.1, ADR-282); unset it for a hardware run";
        guard.finish(Outcome::Refused, why);
        panic!("{why}");
    }
    let ends = match vci_ends(&profile) {
        Ok(ends) => ends,
        Err(why) => {
            guard.finish(Outcome::Refused, &why);
            panic!("{why}");
        }
    };
    let layout = workers(temp.path());
    let options = LaunchOptions::default();

    let mut facts = Vec::new();
    let mut handles = Vec::new();
    for vci in &ends {
        let device = match open_device(vci, &layout, OperatingMode::User, &options).await {
            Ok(device) => device,
            Err(error) => {
                // Without NGR_HW_WORKERS only the build for the host's ABI is installed, and a
                // library of another ABI (a 32-bit DLL on Windows) needs its own worker.
                let why = format!(
                    "the profile's device {:?} did not open: {error}. If no worker is installed \
                     for the library's ABI, set NGR_HW_WORKERS to a worker directory that holds \
                     every ABI's build (design 7.3)",
                    vci.library
                );
                guard.finish(Outcome::Failed, &Masker::for_profile(&profile).text(&why));
                panic!("{why}");
            }
        };
        facts.push(DeviceFacts::of(&device, declared_long_size(vci)));
        // The handle takes the device, so it goes into the report first (design 13.5
        // "Reports").
        guard
            .report()
            .add_device(vci, device.abi(), device.module(), device.version());
        let handle = GuardedHandle::open(
            &profile,
            vci,
            device,
            HandleOptions {
                request_timeout: Duration::from_secs(5),
                response_timeout: Some(FRAME_TIMEOUT),
            },
            &guard,
        )
        .await
        .expect("the guarded handle creates the end's link");
        handles.push(handle);
    }
    let masker = Masker::for_profile(&profile);
    let [first, second]: [GuardedHandle; 2] = handles.try_into().ok().expect("two handles");
    let (mut first, mut second) = (first, second);
    let result = run(
        &profile,
        Ends::TwoModules(&mut first, &mut second),
        &facts,
        &masker,
        FRAME_TIMEOUT,
    )
    .await;
    drop((first, second));
    match result {
        Ok(message) => guard.finish(Outcome::Passed, &message),
        Err(error) => guard.finish(error.outcome(), &masker.text(&error.to_string())),
    }
}

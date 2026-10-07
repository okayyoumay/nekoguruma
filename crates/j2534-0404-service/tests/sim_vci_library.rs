//! `sim-vci`'s device-loss rules (ADR-238, J2534-1 6.10.1), checked by loading the cdylib into
//! this test process through the `j2534-0404` wrapper, with no service in between.
//!
//! `tests/sim_vci_control.rs` drives the same rules through the worker, but there the service
//! may answer for a module it has already seen lose the VCI without calling the library
//! (ADR-131), so it cannot tell whether the library itself keeps the device lost after a
//! replug. This test can, and it runs in CI, where `sim-vci`'s own unit tests do not yet.
//!
//! This file holds a single test, so the process-wide environment it sets cannot race with
//! another test, and the library state it loads is its own.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use j2534_0404::{
    ERR_DEVICE_NOT_CONNECTED, ERR_INVALID_DEVICE_ID, Error, J2534Api0404, StatusCode,
};

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

/// Removes the control directory however the test ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes `command` as the control file `name`; it takes effect at the next J2534 call.
fn send(control_dir: &Path, name: &str, command: &str) {
    let tmp = control_dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, command).expect("control file should be writable");
    std::fs::rename(&tmp, control_dir.join(format!("{name}.json")))
        .expect("control file should be renamed");
}

fn status_of<T: std::fmt::Debug>(result: Result<T, Error>) -> u32 {
    match result {
        Err(Error::ApiStatus {
            code: StatusCode(code),
            ..
        }) => code,
        other => panic!("expected a J2534 error, got {other:?}"),
    }
}

#[test]
fn a_lost_device_stays_lost_until_closed_and_reopens_with_a_new_id() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let control_dir = TempDir(std::env::temp_dir().join(format!("sim-vci-library-{nanos}")));
    std::fs::create_dir(&control_dir.0).expect("control directory should be created");
    // SAFETY: this test binary runs only this test and starts no other thread, so nothing
    // reads the environment concurrently.
    unsafe {
        std::env::set_var("NGR_SIM_VCI_CONTROL_DIR", &control_dir.0);
        std::env::remove_var("NGR_SIM_ECU_CONFIG");
        // `unsigned long` width of the library on this platform.
        let long_size = if cfg!(windows) {
            4
        } else {
            std::mem::size_of::<std::os::raw::c_ulong>()
        };
        std::env::set_var("NGR_J2534_LONG_SIZE", long_size.to_string());
    }
    let api = J2534Api0404::from_path(sim_vci_path()).expect("sim-vci should load");
    let dir = control_dir.0.as_path();

    let device = api.open(None).expect("the device should open");
    api.read_version(device).expect("an open device answers");
    assert_eq!(api.read_vbatt(device).expect("READ_VBATT"), 12_000);

    // Unplugged while open: the device is lost.
    send(dir, "001", r#"{"command": "disconnect_vci"}"#);
    assert_eq!(
        status_of(api.read_version(device)),
        ERR_DEVICE_NOT_CONNECTED
    );
    assert_eq!(status_of(api.read_vbatt(device)), ERR_DEVICE_NOT_CONNECTED);

    // Plugged back in: still lost until the device is closed.
    send(dir, "002", r#"{"command": "connect_vci"}"#);
    assert_eq!(
        status_of(api.read_version(device)),
        ERR_DEVICE_NOT_CONNECTED
    );
    assert_eq!(status_of(api.open(None)), ERR_DEVICE_NOT_CONNECTED);

    // Closing releases it and still reports the loss; the next open gets a new device ID.
    assert_eq!(status_of(api.close(device)), ERR_DEVICE_NOT_CONNECTED);
    let reopened = api.open(None).expect("the device should open again");
    assert_ne!(reopened, device);
    assert_eq!(status_of(api.read_version(device)), ERR_INVALID_DEVICE_ID);
    api.read_version(reopened)
        .expect("the reopened device answers");
    api.close(reopened).expect("the reopened device closes");
    assert!(
        std::fs::read_dir(dir)
            .expect("control directory")
            .next()
            .is_none(),
        "every command should be applied"
    );
}

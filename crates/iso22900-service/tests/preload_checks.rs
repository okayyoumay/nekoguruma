//! The design 7.2 pre-load checks at startup, and what the operating mode makes of a failed one
//! (ADR-281 Decision item 2): `Iso22900Service::new` resolves the library through a test root
//! description file (the debug-only `NGR_PDU_API_ROOT_FILE` override, ADR-282) and checks it
//! before loading. A copy of the mock that regular users can write fails the writability check
//! (as does anything a non-system owner holds, which is every test file on CI): device mode
//! refuses it, user mode reports the findings and loads it. `preload.rs`'s unit tests trust the
//! test user and show a clean library accepted in device mode.

#![cfg(debug_assertions)]

use std::path::{Path, PathBuf};

use iso22900_service::service::Iso22900Service;
use serial_test::serial;
use tonic::{Code, Status};
use vci_service_launcher::vci_server::VciServer;

const LIBRARY_NAME: &str = "PreloadTestLib";

/// A copy of the mock cdylib in a fresh directory, made writable by regular users, and a root
/// description file naming it. The overrides point at them.
struct WritableMock {
    _dir: tempfile::TempDir,
    library: PathBuf,
}

impl WritableMock {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mock = iso22900_mock::mock_library_path()
            .expect("mock cdylib should be discoverable after build");
        let library = dir.path().join(iso22900_mock::mock_library_file_name());
        std::fs::copy(&mock, &library).expect("copy the mock library");
        make_writable_by_regular_users(&library);
        let root_file = iso22900_mock::write_root_file(dir.path(), LIBRARY_NAME, &library)
            .expect("root description file should be writable");
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "").expect("config file should be writable");
        // SAFETY: every test here is `#[serial]`, so nothing reads the environment meanwhile.
        unsafe {
            std::env::set_var("NGR_PDU_API_ROOT_FILE", &root_file);
            std::env::set_var("VCI_CONFIG_PATH", &config);
        }
        Self { _dir: dir, library }
    }
}

#[cfg(unix)]
fn make_writable_by_regular_users(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
        .expect("make the library world-writable");
}

#[cfg(windows)]
fn make_writable_by_regular_users(path: &Path) {
    // BUILTIN\Users gets write access to the file.
    let status = std::process::Command::new("icacls")
        .arg(path)
        .arg("/grant")
        .arg("*S-1-5-32-545:(W)")
        .status()
        .expect("run icacls");
    assert!(status.success(), "icacls failed: {status}");
}

async fn start(mode: &str) -> Result<Iso22900Service, Box<dyn std::error::Error>> {
    let config = Iso22900Service::get_startup_config([
        "iso22900-service".to_owned(),
        format!("iso22900:{LIBRARY_NAME}?mode={mode}"),
    ])
    .expect("startup config");
    Iso22900Service::new(
        config,
        vci_service_launcher::ConfigFile::Checked,
        tokio::sync::watch::channel(false).1,
    )
    .await
}

#[tokio::test]
#[serial]
async fn device_mode_refuses_a_library_that_fails_a_check() {
    let mock = WritableMock::new();
    let error = start("device")
        .await
        .err()
        .expect("device mode should refuse the library");
    let status = error
        .downcast_ref::<Status>()
        .expect("the refusal is a gRPC status");
    assert_eq!(status.code(), Code::FailedPrecondition, "{status}");
    assert!(
        status.message().contains("pre-load checks"),
        "{}",
        status.message()
    );
    // The client message names no local path (ADR-089).
    assert!(
        !status.message().contains(&*mock.library.to_string_lossy()),
        "{}",
        status.message()
    );
}

#[tokio::test]
#[serial]
async fn user_mode_reports_and_loads_a_library_that_fails_a_check() {
    let _mock = WritableMock::new();
    start("user")
        .await
        .expect("user mode should load the library despite the findings");
}

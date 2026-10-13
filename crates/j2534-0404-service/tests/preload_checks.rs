//! The design 7.2 pre-load checks at startup, and what the operating mode makes of a failed one
//! (ADR-281 Decision item 2): `J2534Service::new` resolves the library through a test
//! registration definition (the debug-only `NGR_J2534_DEFINITION_DIR` override, ADR-282) and
//! checks it before loading. A copy of the mock that regular users can write fails the
//! writability check (as does anything a non-system owner holds, which is every test file on
//! CI): device mode refuses it, user mode reports the findings and loads it. `preload.rs`'s unit
//! tests trust the test user and show a clean library accepted in device mode.
//!
//! The name the service is started with is also the key of its per-library settings: a test
//! resolves a definition and reads that library's `modules` under the same name.

#![cfg(debug_assertions)]

use std::path::{Path, PathBuf};

use j2534_0404_service::service::J2534Service;
use serial_test::serial;
use tonic::{Code, Status};
use vci_service_interface::{GetModuleIdsRequest, vci_service_server::VciService};
use vci_service_launcher::vci_server::VciServer;

/// Contains a space and upper-case letters, so a lookup that changed the name would miss it.
const LIBRARY_NAME: &str = "Preload Test VCI";

/// A copy of the mock cdylib in a fresh directory, a registration definition naming it and a
/// configuration file holding `config_toml`. The overrides point at them.
struct TestLibrary {
    _dir: tempfile::TempDir,
    library: PathBuf,
}

impl TestLibrary {
    fn new(config_toml: &str) -> Self {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mock = j2534_0404_mock::mock_library_path()
            .expect("mock cdylib should be discoverable after build");
        let library = dir
            .path()
            .join(mock.file_name().expect("mock library file name"));
        std::fs::copy(&mock, &library).expect("copy the mock library");
        let definitions = dir.path().join("definitions");
        std::fs::create_dir(&definitions).expect("create the definition directory");
        j2534_0404_mock::write_definition(&definitions, LIBRARY_NAME, &library)
            .expect("definition file should be writable");
        let config = dir.path().join("config.toml");
        std::fs::write(&config, config_toml).expect("config file should be writable");
        // SAFETY: every test here is `#[serial]`, so nothing reads the environment meanwhile.
        unsafe {
            std::env::set_var("NGR_J2534_DEFINITION_DIR", &definitions);
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

async fn start(mode: &str) -> Result<J2534Service, Box<dyn std::error::Error>> {
    let config = J2534Service::get_startup_config([
        "j2534-0404-service".to_owned(),
        format!("j2534-0404:{LIBRARY_NAME}?mode={mode}"),
    ])
    .expect("startup config");
    J2534Service::new(config, tokio::sync::watch::channel(false).1).await
}

#[tokio::test]
#[serial]
async fn device_mode_refuses_a_library_that_fails_a_check() {
    let test_library = TestLibrary::new("");
    make_writable_by_regular_users(&test_library.library);
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
        !status
            .message()
            .contains(&*test_library.library.to_string_lossy()),
        "{}",
        status.message()
    );
}

#[tokio::test]
#[serial]
async fn user_mode_reports_and_loads_a_library_that_fails_a_check() {
    let test_library = TestLibrary::new("");
    make_writable_by_regular_users(&test_library.library);
    start("user")
        .await
        .expect("user mode should load the library despite the findings");
}

#[tokio::test]
#[serial]
async fn the_resolved_name_is_the_key_of_the_library_settings() {
    let _test_library = TestLibrary::new(&format!(
        "[[config.apis.j2534-0404.libs.{LIBRARY_NAME:?}.modules]]\n\
         label = \"Bench 1\"\n\
         pname = \"USB:1\"\n"
    ));
    let service = start("user").await.expect("the library should load");
    let rows = service
        .get_module_ids(tonic::Request::new(GetModuleIdsRequest {}))
        .await
        .expect("get_module_ids should succeed")
        .into_inner()
        .module_id_list
        .expect("module list")
        .module_data;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].vendor_module_name, "Bench 1");
    assert_eq!(rows[0].vendor_additional_info, LIBRARY_NAME);
}

//! Verifies that `enumerate_libraries` merges RDF-based auto-discovery with
//! `library_path` entries from the config file (see ADR-036). Uses a
//! runtime `VCI_CONFIG_PATH` override (an absolute path bypasses
//! `config_root()` entirely) to control config content deterministically.
//!
//! Lives in its own integration-test binary, not `src/lib.rs`'s
//! `#[cfg(test)] mod tests`, because it mutates a process-global
//! environment variable that `vci_service_config` reads on every call;
//! keeping it separate avoids racing other tests in the same process (see
//! `vci-service-config/tests/runtime_config_path_override.rs`).

// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]

use std::fs;
use std::path::PathBuf;

use iso22900_registry::{LibrarySource, RegistryViewMode, enumerate_libraries};

#[test]
fn config_only_library_appears_with_config_source() {
    let temp = tempfile::tempdir().expect("create temp dir");
    let config_file = temp.path().join("config.toml");
    fs::write(
        &config_file,
        r#"
        [config.apis.iso22900.libs."ConfigOnlyLib"]
        library_path = "/opt/vci/config-only.so"
        "#,
    )
    .expect("write test config");

    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_file);
    }
    let libs = enumerate_libraries(RegistryViewMode::Native);
    unsafe {
        std::env::remove_var("VCI_CONFIG_PATH");
    }

    let lib = libs
        .iter()
        .find(|l| l.short_name == "ConfigOnlyLib")
        .expect("config-only library should be present");
    assert_eq!(lib.source, LibrarySource::Config);
    assert_eq!(lib.library_path, PathBuf::from("/opt/vci/config-only.so"));
    assert!(lib.description.is_none());
}

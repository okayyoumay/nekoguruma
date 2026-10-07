//! Verifies that a runtime `NGR_J2534_DEFINITION_DIR` environment variable overrides the
//! build-time registration-definition directory in debug builds (ADR-228, ADR-073), so tests
//! can point it at a directory of their own.
//!
//! This lives in its own integration-test binary because it mutates a process-global
//! environment variable, which would race with the lib's unit tests running on other threads.

// Relies on the debug-only runtime NGR_J2534_DEFINITION_DIR override; see ADR-073.
#![cfg(debug_assertions)]

#[test]
fn runtime_env_var_overrides_build_time_definition_dir() {
    let temp = tempfile::tempdir().expect("create temp dir");
    let dir = temp.path().join("j2534");

    // SAFETY: this test binary runs only this test, so nothing reads the environment
    // concurrently.
    unsafe {
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &dir);
    }
    let overridden = vci_service_config::j2534_definition_dir();
    unsafe {
        std::env::remove_var("NGR_J2534_DEFINITION_DIR");
    }
    assert_eq!(overridden, dir);

    // An empty value counts as unset.
    unsafe {
        std::env::set_var("NGR_J2534_DEFINITION_DIR", "");
    }
    let empty = vci_service_config::j2534_definition_dir();
    unsafe {
        std::env::remove_var("NGR_J2534_DEFINITION_DIR");
    }
    assert_eq!(empty, vci_service_config::j2534_definition_dir());
    assert_ne!(empty, dir);
}

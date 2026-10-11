//! Verifies that a runtime `NGR_PDU_API_ROOT_FILE` environment variable overrides the build-time
//! D-PDU API root file in debug builds (ADR-228, ADR-073).
//!
//! This lives in its own integration-test binary because it mutates a process-global
//! environment variable, which would race with the lib's unit tests running on other threads.

// Relies on the debug-only runtime NGR_PDU_API_ROOT_FILE override; see ADR-073.
#![cfg(debug_assertions)]

#[test]
fn runtime_env_var_overrides_build_time_root_file() {
    let temp = tempfile::tempdir().expect("create temp dir");
    let file = temp.path().join("pdu_api_root.xml");

    // SAFETY: this test binary runs only this test, so nothing reads the environment
    // concurrently.
    unsafe {
        std::env::set_var("NGR_PDU_API_ROOT_FILE", &file);
    }
    let overridden = vci_service_config::pdu_api_root_file();
    unsafe {
        std::env::remove_var("NGR_PDU_API_ROOT_FILE");
    }
    assert_eq!(overridden, file);

    // An empty value counts as unset.
    unsafe {
        std::env::set_var("NGR_PDU_API_ROOT_FILE", "");
    }
    let empty = vci_service_config::pdu_api_root_file();
    unsafe {
        std::env::remove_var("NGR_PDU_API_ROOT_FILE");
    }
    assert_eq!(empty, vci_service_config::pdu_api_root_file());
    assert_ne!(empty, file);
}

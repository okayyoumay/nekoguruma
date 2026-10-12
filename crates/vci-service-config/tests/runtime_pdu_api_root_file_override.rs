//! Verifies that a runtime `NGR_PDU_API_ROOT_FILE` environment variable overrides the build-time
//! D-PDU API root file in debug builds (ADR-228, ADR-073), and that `pdu_api_root_file_override`
//! reports that override alone (ADR-282).
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
    let override_alone = vci_service_config::pdu_api_root_file_override();
    unsafe {
        std::env::set_var("NGR_PDU_API_ROOT_FILE", "test-root.xml");
    }
    let relative = vci_service_config::pdu_api_root_file_override();
    let relative_file = vci_service_config::pdu_api_root_file();
    unsafe {
        std::env::remove_var("NGR_PDU_API_ROOT_FILE");
    }
    let unset = vci_service_config::pdu_api_root_file_override();
    assert_eq!(overridden, file);
    assert_eq!(override_alone, Some(file.clone()));
    // A relative value is resolved against the fixed system directory, as the build-time
    // value is, not against the current directory.
    let relative = relative.expect("a relative override is still an override");
    assert!(relative.is_absolute(), "{}", relative.display());
    assert!(
        relative.ends_with("test-root.xml"),
        "{}",
        relative.display()
    );
    assert_eq!(relative, relative_file);
    // Unset: no override, whatever the build-time default is.
    assert_eq!(unset, None);

    // An empty value counts as unset.
    unsafe {
        std::env::set_var("NGR_PDU_API_ROOT_FILE", "");
    }
    let empty = vci_service_config::pdu_api_root_file();
    let empty_override = vci_service_config::pdu_api_root_file_override();
    unsafe {
        std::env::remove_var("NGR_PDU_API_ROOT_FILE");
    }
    assert_eq!(empty, vci_service_config::pdu_api_root_file());
    assert_ne!(empty, file);
    assert_eq!(empty_override, None);
}

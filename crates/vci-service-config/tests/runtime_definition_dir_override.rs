//! Verifies that a runtime `NGR_J2534_DEFINITION_DIR` environment variable overrides the
//! build-time registration-definition directory in debug builds (ADR-228, ADR-073), so tests
//! can point it at a directory of their own, and that `j2534_definition_dir_override` reports
//! that override alone (ADR-282).
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
    let override_alone = vci_service_config::j2534_definition_dir_override();
    unsafe {
        std::env::set_var("NGR_J2534_DEFINITION_DIR", "test-defs");
    }
    let relative = vci_service_config::j2534_definition_dir_override();
    let relative_dir = vci_service_config::j2534_definition_dir();
    unsafe {
        std::env::remove_var("NGR_J2534_DEFINITION_DIR");
    }
    let unset = vci_service_config::j2534_definition_dir_override();
    assert_eq!(overridden, dir);
    assert_eq!(override_alone, Some(dir.clone()));
    // A relative value is resolved against the fixed system directory, as the build-time
    // value is, not against the current directory.
    let relative = relative.expect("a relative override is still an override");
    assert!(relative.is_absolute(), "{}", relative.display());
    assert!(relative.ends_with("test-defs"), "{}", relative.display());
    assert_eq!(relative, relative_dir);
    // Unset: no override, whatever the build-time default is.
    assert_eq!(unset, None);

    // An empty value counts as unset.
    unsafe {
        std::env::set_var("NGR_J2534_DEFINITION_DIR", "");
    }
    let empty = vci_service_config::j2534_definition_dir();
    let empty_override = vci_service_config::j2534_definition_dir_override();
    unsafe {
        std::env::remove_var("NGR_J2534_DEFINITION_DIR");
    }
    assert_eq!(empty, vci_service_config::j2534_definition_dir());
    assert_ne!(empty, dir);
    assert_eq!(empty_override, None);
}

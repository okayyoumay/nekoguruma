//! Verifies that a runtime `NGR_AGENT_INSTALLATION_FILE` environment variable overrides the
//! build-time location of the agent's installation file in debug builds (ADR-494, ADR-073), and
//! that `agent_installation_file_override` reports that override alone.
//!
//! This lives in its own integration-test binary because it mutates a process-global
//! environment variable, which would race with the lib's unit tests running on other threads.

// Relies on the debug-only runtime NGR_AGENT_INSTALLATION_FILE override; see ADR-073.
#![cfg(debug_assertions)]

#[test]
fn runtime_env_var_overrides_build_time_installation_file() {
    let temp = tempfile::tempdir().expect("create temp dir");
    let file = temp.path().join("agent.toml");

    // SAFETY: this test binary runs only this test, so nothing reads the environment
    // concurrently.
    unsafe {
        std::env::set_var("NGR_AGENT_INSTALLATION_FILE", &file);
    }
    let overridden = vci_service_config::agent_installation_file();
    let override_alone = vci_service_config::agent_installation_file_override();
    unsafe {
        std::env::set_var("NGR_AGENT_INSTALLATION_FILE", "test-agent.toml");
    }
    let relative = vci_service_config::agent_installation_file_override();
    let relative_file = vci_service_config::agent_installation_file();
    unsafe {
        std::env::remove_var("NGR_AGENT_INSTALLATION_FILE");
    }
    let unset = vci_service_config::agent_installation_file_override();
    assert_eq!(overridden, file);
    assert_eq!(override_alone, Some(file.clone()));
    // A relative value is resolved against the fixed system directory, as the build-time
    // value is, not against the current directory.
    let relative = relative.expect("a relative override is still an override");
    assert!(relative.is_absolute(), "{}", relative.display());
    assert!(
        relative.ends_with("test-agent.toml"),
        "{}",
        relative.display()
    );
    assert_eq!(relative, relative_file);
    // Unset: no override, whatever the build-time default is.
    assert_eq!(unset, None);

    // An empty value counts as unset.
    unsafe {
        std::env::set_var("NGR_AGENT_INSTALLATION_FILE", "");
    }
    let empty = vci_service_config::agent_installation_file();
    let empty_override = vci_service_config::agent_installation_file_override();
    unsafe {
        std::env::remove_var("NGR_AGENT_INSTALLATION_FILE");
    }
    assert_eq!(empty, vci_service_config::agent_installation_file());
    assert_ne!(empty, file);
    assert_eq!(empty_override, None);
}

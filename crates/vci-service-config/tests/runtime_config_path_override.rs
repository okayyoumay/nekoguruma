//! Verifies that a runtime `VCI_CONFIG_PATH` environment variable overrides
//! the build-time embedded config path. This is what lets IDE debug launch
//! configurations (see .vscode/launch.json) point at a dedicated config file
//! without a rebuild.
//!
//! This lives in its own integration-test binary (rather than
//! `src/lib.rs`'s `#[cfg(test)] mod tests`) because it mutates a
//! process-global environment variable that `find_logging_config`
//! reads on every call; keeping it in a separate binary avoids racing
//! against the unrelated unit tests in the lib's own test binary, which run
//! concurrently on separate threads within the same process.
//!
//! Only an absolute override is tested here: it bypasses `config_root()`
//! entirely (see `resolve_config_path` in `src/lib.rs`), so this test needs
//! no access to `src/lib.rs`'s private, test-only `config_root()` override.

// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]

use std::fs;

#[test]
#[expect(
    unsafe_code,
    reason = "`std::env::set_var`/`remove_var` are unsafe (edition 2024) and have no safe form (ADR-502)"
)]
fn runtime_env_var_overrides_build_time_config_path() {
    let temp = tempfile::tempdir().expect("create temp dir");
    let config_file = temp.path().join("runtime-override-config.toml");
    fs::write(
        &config_file,
        r#"
        [config.logging]
        level = "trace"
        "#,
    )
    .expect("write test config");

    // SAFETY: this test binary runs only this test, so nothing reads the environment
    // concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_file);
    }
    let cfg = vci_service_config::find_logging_config("iso22900", None, "somelib");
    // SAFETY: this test binary runs only this test, so nothing reads the environment
    // concurrently.
    unsafe {
        std::env::remove_var("VCI_CONFIG_PATH");
    }

    assert_eq!(cfg.level.as_deref(), Some("trace"));
}

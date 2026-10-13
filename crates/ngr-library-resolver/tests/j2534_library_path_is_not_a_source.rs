//! A `library_path` entry in `config.toml` does not resolve a J2534 name (ADR-228 Decision item 4,
//! ADR-281 Decision item 1): the shared resolver is the only J2534 lookup, and it reads only the
//! registration definitions (the registry on Windows). The debug-only overrides point the
//! resolver at an empty definition directory and the configuration at a file that names a
//! library for the name.
//!
//! This lives in its own integration-test binary because it sets process-global environment
//! variables, which would race with the lib's unit tests running on other threads.

#![cfg(debug_assertions)]

use ngr_library_resolver::j2534_0404::{self, ResolveError};

#[test]
fn a_configured_library_path_does_not_resolve_a_name() {
    const NAME: &str = "__ngr_library_path_only__";
    let dir = tempfile::tempdir().expect("create temp dir");
    let definitions = dir.path().join("definitions");
    std::fs::create_dir(&definitions).expect("create the definition directory");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[config.apis.j2534-0404.libs.\"{NAME}\"]\nlibrary_path = '{}'\n",
            dir.path().join("vendor.so").display()
        ),
    )
    .expect("write config file");

    // SAFETY: this test binary runs only this test, so nothing reads the environment
    // concurrently.
    unsafe {
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &definitions);
        std::env::set_var("VCI_CONFIG_PATH", &config);
    }
    let resolved = j2534_0404::resolve(NAME);
    unsafe {
        std::env::remove_var("NGR_J2534_DEFINITION_DIR");
        std::env::remove_var("VCI_CONFIG_PATH");
    }

    assert!(
        matches!(resolved, Err(ResolveError::NotFound { .. })),
        "{resolved:?}"
    );
}

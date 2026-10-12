//! On Windows, a debug build with a runtime `NGR_J2534_DEFINITION_DIR` resolves J2534 names in
//! that directory instead of the registry, through the public `resolve` and
//! `resolve_on_registry` themselves (ADR-282).
//!
//! This lives in its own integration-test binary because it sets a process-global environment
//! variable, which would race with the lib's unit tests running on other threads.

#![cfg(all(windows, debug_assertions))]

use j2534_0404_registry::RegistryViewMode;
use ngr_library_resolver::j2534_0404::{self, Source};

#[test]
fn the_runtime_override_replaces_the_registry() {
    const NAME: &str = "__ngr_windows_override_only__";
    let dir = tempfile::tempdir().expect("create temp dir");
    let library = dir.path().join("mock.dll");
    let file = dir.path().join("override.toml");
    std::fs::write(
        &file,
        format!(
            "Name = \"{NAME}\"\nFunctionLibrary = '{}'\n",
            library.display()
        ),
    )
    .expect("write definition");

    // SAFETY: this test binary runs only this test, so nothing reads the environment
    // concurrently.
    unsafe {
        std::env::set_var("NGR_J2534_DEFINITION_DIR", dir.path());
    }
    let native = j2534_0404::resolve(NAME);
    let all = j2534_0404::resolve_on_registry(NAME, RegistryViewMode::All);
    unsafe {
        std::env::remove_var("NGR_J2534_DEFINITION_DIR");
    }
    let unset = j2534_0404::resolve(NAME);

    for resolved in [native, all] {
        let resolved = resolved.expect("resolved through the override");
        assert_eq!(resolved.source, Source::Definition(file.clone()));
        assert_eq!(resolved.definition.library, library);
    }
    // Without the override the registry is read, which does not carry the name.
    assert!(unset.is_err(), "{unset:?}");
}

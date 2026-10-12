//! On Windows, a debug build with a runtime `NGR_PDU_API_ROOT_FILE` takes the D-PDU API root
//! description file from it instead of the registry, through the public `root_file_path` and
//! `resolve` themselves (ADR-282).
//!
//! This lives in its own integration-test binary because it sets a process-global environment
//! variable, which would race with the lib's unit tests running on other threads.

#![cfg(all(windows, debug_assertions))]

use ngr_library_resolver::RegistryView;
use ngr_library_resolver::iso22900;

#[test]
fn the_runtime_override_replaces_the_registry() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let file = dir.path().join("pdu_api_root.xml");

    // SAFETY: this test binary runs only this test, so nothing reads the environment
    // concurrently.
    unsafe {
        std::env::set_var("NGR_PDU_API_ROOT_FILE", &file);
    }
    let views = [RegistryView::Wow64_32, RegistryView::Wow64_64]
        .map(|view| (view, iso22900::root_file_path(view)));
    // The file does not exist yet: `resolve` reports that the override's root file is missing,
    // which shows it looked there and not in the registry.
    let missing = iso22900::resolve("__ngr_override_only__");
    unsafe {
        std::env::remove_var("NGR_PDU_API_ROOT_FILE");
    }
    let unset = iso22900::root_file_path(RegistryView::native());

    for (view, path) in views {
        assert_eq!(
            path.expect("no registry read"),
            Some(file.clone()),
            "{view:?}"
        );
    }
    assert!(
        matches!(
            &missing,
            Err(iso22900::ResolveError::NoRootFile { path: Some(path) }) if *path == file
        ),
        "{missing:?}"
    );
    // Without the override the registry is read, which does not name this file.
    assert_ne!(unset.ok().flatten(), Some(file));
}

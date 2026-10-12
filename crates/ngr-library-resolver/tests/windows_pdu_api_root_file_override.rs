//! On Windows, a debug build with a runtime `NGR_PDU_API_ROOT_FILE` takes the D-PDU API root
//! description file from it instead of the registry, through the public `root_file_path` and
//! `resolve` themselves, with the same root description file a Linux test writes (ADR-282).
//!
//! This lives in its own integration-test binary because it sets a process-global environment
//! variable, which would race with the lib's unit tests running on other threads.

#![cfg(all(windows, debug_assertions))]

use ngr_library_resolver::RegistryView;
use ngr_library_resolver::iso22900;

#[test]
fn the_runtime_override_replaces_the_registry() {
    const NAME: &str = "__NGR_OVERRIDE_ONLY__";
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
    let missing = iso22900::resolve(NAME);
    // The same root description file a Linux test writes, naming the library by a `file:` URI.
    let library = dir.path().join("mock.dll");
    let library_uri = format!(
        "file:///{}",
        library.display().to_string().replace('\\', "/")
    );
    std::fs::write(
        &file,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<MVCI_PDU_API_ROOT><MVCI_PDU_API>\
             <SHORT_NAME>{NAME}</SHORT_NAME><LIBRARY_FILE URI=\"{library_uri}\"/>\
             </MVCI_PDU_API></MVCI_PDU_API_ROOT>"
        ),
    )
    .expect("write root file");
    let found = iso22900::resolve(NAME);
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
    // Once the file exists, the name resolves through it, and the override file is a naming
    // file of the result, so the 7.2 checks cover it (ADR-282 Decision item 4).
    let found = found.expect("resolved through the override's root file");
    assert_eq!(found.root_file, file);
    assert_eq!(found.implementation.library_file, library);
    assert_eq!(found.naming_files().first(), Some(&file));
    // Without the override the registry is read, which does not name this file.
    assert_ne!(unset.ok().flatten(), Some(file));
}

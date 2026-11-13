//! Locates build artifacts of other packages (the `sim-vci` cdylib) for the tests of this
//! package, shared through `#[path]`. The lookup assumes no build-directory layout: it searches
//! the test executable's target directory (`crates/agent/tests/support/build_layout.rs`).

use std::path::PathBuf;

#[path = "../../../agent/tests/support/build_layout.rs"]
mod build_layout;

/// The `sim-vci` cdylib built for this test run. `sim-vci` is a dev-dependency, so it is a
/// product of the same build as the test executable.
pub fn sim_vci_path() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let name = format!(
        "{}sim_vci{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    build_layout::find_artifact(&exe, &name).unwrap_or_else(|| {
        panic!(
            "{name} should be built (not found under {})",
            build_layout::target_scope(&exe).display()
        )
    })
}

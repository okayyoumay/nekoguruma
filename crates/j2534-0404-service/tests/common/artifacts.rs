//! Locates build artifacts of other packages (the `sim-vci` cdylib) for the tests of this
//! package, shared through `#[path]`. Cargo is asked for the path
//! (`crates/agent/tests/support/cargo_artifact.rs`), so no target-directory layout is assumed.

use std::path::PathBuf;

#[path = "../../../agent/tests/support/cargo_artifact.rs"]
mod cargo_artifact;

/// The `sim-vci` cdylib, up to date, built with the test's profile and target. `sim-vci` is a
/// dev-dependency that asks for no features, so the nested build needs none either.
pub fn sim_vci_path() -> PathBuf {
    cargo_artifact::build_artifact("sim-vci", cargo_artifact::Artifact::Cdylib)
}

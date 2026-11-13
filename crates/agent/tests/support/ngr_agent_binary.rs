//! The `ngr-agent` binary for the end-to-end tests of the worker packages, shared through
//! `#[path]`: `crates/j2534-0404-service/tests/ngr_agent_end_to_end.rs`,
//! `crates/iso22900-service/tests/ngr_agent_dpdu_end_to_end.rs`,
//! `crates/iso22900-service/tests/ngr_agent_definition_end_to_end.rs` and
//! `crates/iso22900-service/tests/ngr_agent_dpdu_unbound_indications.rs`, and the server's
//! `crates/server/tests/postgres/agent_register.rs`.
//!
//! `ngr-agent` belongs to another package, so a run of the package under test alone does not
//! build it, or rebuilds it only when the run also builds the `agent` package's binaries. The
//! path is therefore taken from Cargo itself (`cargo_artifact.rs`): the test builds
//! `-p agent --bin ngr-agent` and uses the path Cargo reports. Cargo rebuilds the binary when
//! its sources or those of the crates it links changed, so the test never runs an older agent
//! than the sources it was built from.

use std::path::PathBuf;

#[path = "cargo_artifact.rs"]
pub mod cargo_artifact;

/// The up-to-date `ngr-agent` binary of the test's profile and target.
pub fn ngr_agent_path() -> PathBuf {
    cargo_artifact::build_artifact("agent", cargo_artifact::Artifact::Bin("ngr-agent"))
}

//! The lookup of another package's build output the end-to-end tests share
//! (`crates/agent/tests/support/cargo_artifact.rs`): which Cargo command the nested build runs,
//! how its JSON messages are read, and that the artifacts it returns exist.

#[path = "../../agent/tests/support/ngr_agent_binary.rs"]
mod ngr_agent_binary;

use std::path::{Path, PathBuf};

use ngr_agent_binary::cargo_artifact::{
    Artifact, build_artifact, cargo_args, cargo_target, parse_artifact, workspace_root,
};

const AGENT_MESSAGE: &str = r#"{"reason":"compiler-artifact","package_id":"path+file:///w/crates/agent#0.1.0","manifest_path":"/w/crates/agent/Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"agent"},"filenames":["/t/debug/deps/libagent-0a.rlib"],"executable":null,"fresh":false}"#;
const BIN_MESSAGE: &str = r#"{"reason":"compiler-artifact","package_id":"path+file:///w/crates/agent#0.1.0","manifest_path":"/w/crates/agent/Cargo.toml","target":{"kind":["bin"],"crate_types":["bin"],"name":"ngr-agent"},"filenames":["/t/debug/build/agent/1b/out/ngr-agent"],"executable":"/t/debug/ngr-agent","fresh":true}"#;
const OTHER_BIN: &str = r#"{"reason":"compiler-artifact","package_id":"x","manifest_path":"/w/crates/server/Cargo.toml","target":{"kind":["bin"],"crate_types":["bin"],"name":"ngr-agent"},"filenames":["/t/debug/other"],"executable":"/t/debug/other","fresh":true}"#;
const OK: &str = r#"{"reason":"build-finished","success":true}"#;
const FAILED: &str = r#"{"reason":"build-finished","success":false}"#;

fn cdylib_message() -> String {
    let file = format!(
        "{}sim_vci{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    format!(
        r#"{{"reason":"compiler-artifact","package_id":"path+file:///w/crates/sim-vci#0.1.0","manifest_path":"/w/crates/sim-vci/Cargo.toml","target":{{"kind":["cdylib"],"crate_types":["cdylib"],"name":"sim_vci"}},"filenames":["/t/debug/{file}.d","/t/debug/{file}"],"executable":null,"fresh":false}}"#
    )
}

#[test]
fn a_binarys_executable_is_the_one_of_its_package_and_target() {
    let stdout = format!("{AGENT_MESSAGE}\n{OTHER_BIN}\nnot json\n{BIN_MESSAGE}\n{OK}\n");
    assert_eq!(
        parse_artifact(&stdout, "agent", Artifact::Bin("ngr-agent")),
        Ok(PathBuf::from("/t/debug/ngr-agent"))
    );
    // The same target name in another package, and another target of the package, do not count.
    let error = parse_artifact(
        &format!("{AGENT_MESSAGE}\n{OTHER_BIN}\n{OK}\n"),
        "agent",
        Artifact::Bin("ngr-agent"),
    );
    assert!(error.is_err(), "{error:?}");
}

#[test]
fn a_cdylibs_path_is_the_file_with_the_platforms_prefix_and_suffix() {
    let stdout = format!("{AGENT_MESSAGE}\n{}\n{OK}\n", cdylib_message());
    let path = parse_artifact(&stdout, "sim-vci", Artifact::Cdylib).expect("the cdylib");
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with(std::env::consts::DLL_PREFIX)
            && name.ends_with(std::env::consts::DLL_SUFFIX),
        "{name}"
    );
}

#[test]
fn a_failed_or_unfinished_build_gives_no_path() {
    let stdout = format!("{BIN_MESSAGE}\n{FAILED}\n");
    let error = parse_artifact(&stdout, "agent", Artifact::Bin("ngr-agent")).unwrap_err();
    assert!(error.contains("failed"), "{error}");
    let error = parse_artifact(BIN_MESSAGE, "agent", Artifact::Bin("ngr-agent")).unwrap_err();
    assert!(error.contains("build-finished"), "{error}");
}

#[test]
fn the_nested_build_names_the_package_target_and_configuration() {
    let manifest = Path::new("/w/Cargo.toml");
    let args = cargo_args("agent", Artifact::Bin("ngr-agent"), false, None, manifest);
    assert_eq!(
        args,
        [
            "build",
            "--locked",
            "--message-format=json",
            "--manifest-path",
            "/w/Cargo.toml",
            "-p",
            "agent",
            "--bin",
            "ngr-agent"
        ]
    );
    let args = cargo_args(
        "sim-vci",
        Artifact::Cdylib,
        true,
        Some("aarch64-unknown-linux-gnu"),
        manifest,
    );
    assert_eq!(
        args[5..],
        [
            "-p",
            "sim-vci",
            "--lib",
            "--release",
            "--target",
            "aarch64-unknown-linux-gnu"
        ]
    );
}

#[test]
fn a_target_is_passed_only_below_a_triple_directory() {
    let root = Path::new("/w/target");
    for host in [
        "/w/target/debug/deps/e2e-0a",
        "/w/target/debug/build/pkg/0a/out/e2e-0a",
        "/w/target/release-lto/deps/e2e-0a",
        "/elsewhere/debug/deps/e2e-0a",
    ] {
        assert_eq!(cargo_target(Path::new(host), root), None, "{host}");
    }
    assert_eq!(
        cargo_target(
            Path::new("/w/target/x86_64-pc-windows-gnullvm/debug/deps/e2e-0a"),
            root
        )
        .as_deref(),
        Some("x86_64-pc-windows-gnullvm")
    );
}

#[test]
fn the_built_artifacts_exist() {
    let sim_vci = build_artifact("sim-vci", Artifact::Cdylib);
    assert!(sim_vci.is_file(), "{}", sim_vci.display());
    let agent = ngr_agent_binary::ngr_agent_path();
    assert!(agent.is_file(), "{}", agent.display());
    // A second call answers from the cache with the same path.
    assert_eq!(build_artifact("sim-vci", Artifact::Cdylib), sim_vci);
    assert!(workspace_root().join("Cargo.toml").is_file());
}

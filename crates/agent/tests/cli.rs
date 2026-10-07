//! `ngr-agent` command-line errors. A run against a worker is tested end to end in
//! `crates/j2534-0404-service/tests/ngr_agent_end_to_end.rs`, which also relies on this file:
//! cargo builds the `ngr-agent` binary for a package's integration tests.

use std::process::Command;

fn ngr_agent() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ngr-agent"))
}

#[test]
fn usage_error_exits_with_2() {
    let output = ngr_agent().output().expect("ngr-agent should run");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage: ngr-agent run"));
    assert!(output.stdout.is_empty());
}

#[test]
fn unreadable_program_exits_with_1_before_launching_a_worker() {
    let output = ngr_agent()
        .args(["run", "--vci", "none", "--program"])
        .arg(std::env::temp_dir().join("ngr-agent-cli-test-missing-program.json"))
        .output()
        .expect("ngr-agent should run");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot read"));
    assert!(output.stdout.is_empty());
}

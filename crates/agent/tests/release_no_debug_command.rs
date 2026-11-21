//! A release `ngr-agent` has no debug commands (ADR-497): `debug fetch` is refused as an unknown
//! command at the parser, before the installation file, a key or the network is touched. The file
//! is compiled in release builds only, since a debug build has the command.

#![cfg(not(debug_assertions))]

use std::process::Command;

#[test]
fn a_release_build_refuses_debug_fetch_as_an_unknown_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_ngr-agent"))
        .args([
            "debug",
            "fetch",
            "--mode",
            "user",
            "--instruction",
            "i.bin",
            "--kind",
            "document",
            "--sha256",
            &"0".repeat(64),
        ])
        .output()
        .expect("ngr-agent should start");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown command"), "{stderr}");
    assert!(output.stdout.is_empty());
}

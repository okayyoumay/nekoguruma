//! A release `ngr-agent` has no debug commands (ADR-497, ADR-493 item 10): `debug fetch` and `debug
//! enqueue` are refused as unknown commands at the parser, before the installation file, a key, the
//! ledger or the network is touched. The file
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

#[test]
fn a_release_build_refuses_debug_enqueue_as_an_unknown_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_ngr-agent"))
        .args([
            "debug",
            "enqueue",
            "--mode",
            "user",
            "--job",
            "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f",
            "--generation",
            "1",
            "--kind",
            "result",
            "--file",
            "result.json",
        ])
        .output()
        .expect("ngr-agent should start");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown command"), "{stderr}");
    assert!(output.stdout.is_empty());
}

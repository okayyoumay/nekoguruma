//! The startup argument's query rules (`docs/grpc-instance-spec.md`,
//! "Startup Argument"), checked through the real binary: unknown query
//! parameters are ignored, while a repeated or malformed `port` is rejected.
//!
//! A rejected argument does not end the process: `vci-service-launcher` keeps
//! the stdio JSON-RPC server running so the caller can read the failure, and
//! `get_status` then reports no library name and the parse error. An accepted
//! argument reports its library name. Each test names a library that is not
//! configured anywhere, so the service never loads one.

use std::process::Stdio;

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::{Duration, timeout},
};

const LIBRARY_NAME: &str = "ngr-startup-argument-test-unconfigured";

/// Starts the service with `startup_arg`, reads its `get_status` result, and
/// stops it again.
async fn status_for(startup_arg: &str) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_iso22900-service"))
        .arg(startup_arg)
        .env_remove("VCI_CONFIG_PATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("iso22900-service child process should spawn");
    let mut stdin = child.stdin.take().expect("child stdin should be piped");
    let mut stdout =
        BufReader::new(child.stdout.take().expect("child stdout should be piped")).lines();

    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"get_status\"}\n")
        .await
        .expect("status request should be written");
    let line = timeout(Duration::from_secs(10), stdout.next_line())
        .await
        .expect("status response should arrive in time")
        .expect("status read should succeed")
        .expect("status response line should exist");
    let status: Value = serde_json::from_str(&line).expect("status response should be JSON");

    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"stop\"}\n")
        .await
        .expect("stop request should be written");
    stdin.flush().await.expect("stop request should flush");
    let exit = timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("child should exit after stop")
        .expect("child wait should succeed");
    assert!(exit.success(), "child should exit successfully: {exit}");

    status["result"].clone()
}

fn error_text(status: &Value) -> &str {
    status["error"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn unknown_query_parameters_are_ignored() {
    let status = status_for(&format!(
        "iso22900:{LIBRARY_NAME}?profile=test&port=0&verbose"
    ))
    .await;
    assert_eq!(status["library_name"], LIBRARY_NAME, "{status}");
    assert!(
        !error_text(&status).contains("startup argument"),
        "the argument should have been accepted: {status}"
    );
}

#[tokio::test]
async fn a_repeated_port_is_rejected() {
    let status = status_for(&format!("iso22900:{LIBRARY_NAME}?port=0&port=0")).await;
    assert_eq!(status["running"], false, "{status}");
    assert_eq!(status["library_name"], Value::Null, "{status}");
    assert!(
        error_text(&status).contains("must not repeat the port"),
        "{status}"
    );
}

#[tokio::test]
async fn a_port_that_is_not_a_u16_is_rejected() {
    for port in ["65536", "-1", "abc"] {
        let status = status_for(&format!("iso22900:{LIBRARY_NAME}?port={port}")).await;
        assert_eq!(status["running"], false, "port={port}: {status}");
        assert_eq!(status["library_name"], Value::Null, "port={port}: {status}");
        assert!(
            error_text(&status).contains("valid u16"),
            "port={port}: {status}"
        );
    }
}

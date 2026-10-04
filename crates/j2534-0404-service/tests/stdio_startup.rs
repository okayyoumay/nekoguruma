// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! Full stdio lifecycle integration coverage for `j2534-0404-service`: ping,
//! get_status, stop, and process exit, driven against the real compiled
//! binary over its piped stdin/stdout. Ported from
//! `iso22900-service/tests/stdio_startup.rs` -- the JSON-RPC-over-stdio
//! machinery both services use is shared, implemented once in
//! `vci-service-launcher` (see `docs/startup-spec.md` for the exact
//! request/response contract this file verifies).

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use j2534_0404_mock::mock_library_path;
use serial_test::serial;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::{Duration, timeout},
};
use vci_service_interface::{
    ComLogicalLinkHandle, CreateComLogicalLinkRequest, GetModuleIdsRequest, GetResourceIdsRequest,
    ModuleConnectRequest, ModuleHandle, ResourceData, SubscribeEventRequest,
    create_com_logical_link_request, resource_data, vci_service_client::VciServiceClient,
};

const VCI_CONFIG_PATH_ENV: &str = "VCI_CONFIG_PATH";
const TEST_LIBRARY_NAME: &str = "mock-stdio-library";

fn unique_temp_path(file_name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("j2534-0404-service-{nanos}-{file_name}"))
}

/// Writes a `config.toml` that points `TEST_LIBRARY_NAME`'s `library_path`
/// at the compiled mock cdylib, so the spawned service resolves it without
/// needing the Windows registry.
fn write_test_library_config() -> PathBuf {
    let mock_library =
        mock_library_path().expect("mock library should be discoverable after build");
    let config_path = unique_temp_path("config.toml");
    let config_toml = format!(
        "[config.apis.j2534-0404.libs.{TEST_LIBRARY_NAME:?}]\nlibrary_path = {:?}\n",
        mock_library.display().to_string()
    );
    std::fs::write(&config_path, config_toml).expect("test config file should be writable");
    config_path
}

/// GetModuleIds -> ModuleConnect -> GetResourceIds("ISO15765") ->
/// CreateComLogicalLink, mirroring `tests/grpc_mock/lifecycle.rs`'s proven
/// minimal setup sequence for this crate's RPC surface. No
/// `ConnectComLogicalLink` is needed here -- `SubscribeEvent` only requires
/// the CLL to exist, not to be connected.
async fn create_default_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
) -> ComLogicalLinkHandle {
    let modules = client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect("get_module_ids should succeed")
        .into_inner();
    let module_handle: ModuleHandle = modules
        .module_id_list
        .and_then(|list| list.module_data.into_iter().next())
        .and_then(|entry| entry.module_handle)
        .expect("module handle should be present");

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("module_connect should succeed");

    let resource_ids = client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(module_handle),
            resource_data: Some(ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(resource_data::Protocol::ProtocolName(
                    "ISO15765".to_string(),
                )),
            }),
        })
        .await
        .expect("get_resource_ids should succeed")
        .into_inner();
    let resource_id = resource_ids
        .resource_id_list
        .and_then(|list| list.resource_id_data_array.into_iter().next())
        .and_then(|data| data.resource_id_array.into_iter().next())
        .expect("a resource id should resolve for protocol name ISO15765");

    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(module_handle),
            resource: Some(create_com_logical_link_request::Resource::ResourceId(
                resource_id,
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

#[tokio::test]
#[serial]
async fn startup_argument_auto_starts_grpc_and_stdio_can_stop_process() {
    let config_path = write_test_library_config();
    let exe = env!("CARGO_BIN_EXE_j2534-0404-service");

    let mut child = Command::new(exe)
        .arg(format!("j2534-0404:{TEST_LIBRARY_NAME}"))
        .env(VCI_CONFIG_PATH_ENV, &config_path)
        // These tests exercise stdio/gRPC startup mechanics directly (no
        // manager involved to push an auth key via `set_auth_key`), so
        // disable ADR-221's listener auth the same way a manager-less dev
        // run would (see `vci-service-launcher::vci_server`'s doc comment
        // on this variable; this file is already `#![cfg(debug_assertions)]`
        // only, matching the variable's own debug-only compile gate).
        .env("VCI_SERVICE_INSECURE_NO_AUTH", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("j2534-0404-service child process should spawn");

    let mut stdin = child.stdin.take().expect("child stdin should be piped");
    let stdout = child.stdout.take().expect("child stdout should be piped");
    let mut stdout = BufReader::new(stdout).lines();

    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":0,"method":"ping"}
"#,
        )
        .await
        .expect("ping request should be written");

    let ping_line = timeout(Duration::from_secs(5), stdout.next_line())
        .await
        .expect("ping response should arrive in time")
        .expect("ping read should succeed")
        .expect("ping response line should exist");
    let ping_response: serde_json::Value =
        serde_json::from_str(&ping_line).expect("ping response should be valid JSON");
    assert_eq!(ping_response["result"]["message"], "pong");

    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"get_status"}
"#,
        )
        .await
        .expect("status request should be written");

    let status_line = timeout(Duration::from_secs(5), stdout.next_line())
        .await
        .expect("status response should arrive in time")
        .expect("status read should succeed")
        .expect("status response line should exist");
    let status: serde_json::Value =
        serde_json::from_str(&status_line).expect("status response should be valid JSON");

    assert_eq!(status["result"]["library_name"], TEST_LIBRARY_NAME);
    assert_eq!(status["result"]["running"], true);

    let endpoint = status["result"]["endpoints"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .filter(|s| s.parse::<std::net::SocketAddr>().is_ok())
        .expect("status response should include endpoints with a valid socket address");

    let mut grpc_client = timeout(
        Duration::from_secs(5),
        VciServiceClient::connect(format!("http://{endpoint}")),
    )
    .await
    .expect("gRPC connection should complete in time")
    .expect("gRPC client should connect to started server");

    let module_ids = grpc_client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect("started gRPC server should answer requests")
        .into_inner();
    assert!(module_ids.module_id_list.is_some());

    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":2,"method":"stop"}
"#,
        )
        .await
        .expect("stop request should be written");
    stdin.flush().await.expect("stop request should flush");

    let stop_line = timeout(Duration::from_secs(5), stdout.next_line())
        .await
        .expect("stop response should arrive in time")
        .expect("stop read should succeed")
        .expect("stop response line should exist");
    let stop_response: serde_json::Value =
        serde_json::from_str(&stop_line).expect("stop response should be valid JSON");
    assert_eq!(stop_response["result"]["stopped"], true);

    let exit_status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("child should exit after stop response")
        .expect("child wait should succeed");
    assert!(
        exit_status.success(),
        "child exit status should be successful"
    );

    let _ = std::fs::remove_file(&config_path);
}

#[tokio::test]
#[serial]
async fn stop_notification_without_id_exits_process_without_response() {
    let config_path = write_test_library_config();
    let exe = env!("CARGO_BIN_EXE_j2534-0404-service");

    let mut child = Command::new(exe)
        .arg(format!("j2534-0404:{TEST_LIBRARY_NAME}"))
        .env(VCI_CONFIG_PATH_ENV, &config_path)
        // These tests exercise stdio/gRPC startup mechanics directly (no
        // manager involved to push an auth key via `set_auth_key`), so
        // disable ADR-221's listener auth the same way a manager-less dev
        // run would (see `vci-service-launcher::vci_server`'s doc comment
        // on this variable; this file is already `#![cfg(debug_assertions)]`
        // only, matching the variable's own debug-only compile gate).
        .env("VCI_SERVICE_INSECURE_NO_AUTH", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("j2534-0404-service child process should spawn");

    let mut stdin = child.stdin.take().expect("child stdin should be piped");
    let stdout = child.stdout.take().expect("child stdout should be piped");
    let mut stdout = BufReader::new(stdout).lines();

    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","method":"stop"}
"#,
        )
        .await
        .expect("stop notification should be written");
    stdin.flush().await.expect("stop notification should flush");

    let next_line = timeout(Duration::from_secs(5), stdout.next_line())
        .await
        .expect("process should close stdout in time")
        .expect("stdout read should succeed");
    assert!(
        next_line.is_none(),
        "stop notification should not produce a response"
    );

    let exit_status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("child should exit after stop notification")
        .expect("child wait should succeed");
    assert!(
        exit_status.success(),
        "child exit status should be successful"
    );

    let _ = std::fs::remove_file(&config_path);
}

#[tokio::test]
#[serial]
async fn process_exits_when_stdin_is_closed() {
    let config_path = write_test_library_config();
    let exe = env!("CARGO_BIN_EXE_j2534-0404-service");

    let mut child = Command::new(exe)
        .arg(format!("j2534-0404:{TEST_LIBRARY_NAME}"))
        .env(VCI_CONFIG_PATH_ENV, &config_path)
        // These tests exercise stdio/gRPC startup mechanics directly (no
        // manager involved to push an auth key via `set_auth_key`), so
        // disable ADR-221's listener auth the same way a manager-less dev
        // run would (see `vci-service-launcher::vci_server`'s doc comment
        // on this variable; this file is already `#![cfg(debug_assertions)]`
        // only, matching the variable's own debug-only compile gate).
        .env("VCI_SERVICE_INSECURE_NO_AUTH", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("j2534-0404-service child process should spawn");

    let mut stdin = child.stdin.take().expect("child stdin should be piped");
    let stdout = child.stdout.take().expect("child stdout should be piped");
    let mut stdout = BufReader::new(stdout).lines();

    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"get_status"}
"#,
        )
        .await
        .expect("status request should be written");

    let status_line = timeout(Duration::from_secs(5), stdout.next_line())
        .await
        .expect("status response should arrive in time")
        .expect("status read should succeed")
        .expect("status response line should exist");
    let status: serde_json::Value =
        serde_json::from_str(&status_line).expect("status response should be valid JSON");
    assert_eq!(status["result"]["running"], true);

    drop(stdin);

    let exit_status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("child should exit after stdin closes")
        .expect("child wait should succeed");
    assert!(
        exit_status.success(),
        "child exit status should be successful"
    );

    let _ = std::fs::remove_file(&config_path);
}

#[tokio::test]
#[serial]
async fn process_exits_when_stdin_is_closed_with_live_subscribe_event_stream() {
    let config_path = write_test_library_config();
    let exe = env!("CARGO_BIN_EXE_j2534-0404-service");

    let mut child = Command::new(exe)
        .arg(format!("j2534-0404:{TEST_LIBRARY_NAME}"))
        .env(VCI_CONFIG_PATH_ENV, &config_path)
        // These tests exercise stdio/gRPC startup mechanics directly (no
        // manager involved to push an auth key via `set_auth_key`), so
        // disable ADR-221's listener auth the same way a manager-less dev
        // run would (see `vci-service-launcher::vci_server`'s doc comment
        // on this variable; this file is already `#![cfg(debug_assertions)]`
        // only, matching the variable's own debug-only compile gate).
        .env("VCI_SERVICE_INSECURE_NO_AUTH", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("j2534-0404-service child process should spawn");

    let mut stdin = child.stdin.take().expect("child stdin should be piped");
    let stdout = child.stdout.take().expect("child stdout should be piped");
    let mut stdout = BufReader::new(stdout).lines();

    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"get_status"}
"#,
        )
        .await
        .expect("status request should be written");

    let status_line = timeout(Duration::from_secs(5), stdout.next_line())
        .await
        .expect("status response should arrive in time")
        .expect("status read should succeed")
        .expect("status response line should exist");
    let status: serde_json::Value =
        serde_json::from_str(&status_line).expect("status response should be valid JSON");
    let endpoint = status["result"]["endpoints"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .filter(|s| s.parse::<std::net::SocketAddr>().is_ok())
        .expect("status response should include endpoints with a valid socket address");

    let mut setup_client = timeout(
        Duration::from_secs(5),
        VciServiceClient::connect(format!("http://{endpoint}")),
    )
    .await
    .expect("gRPC connection should complete in time")
    .expect("gRPC client should connect");
    let cll_handle = create_default_cll(&mut setup_client).await;

    let mut stream_client = timeout(
        Duration::from_secs(5),
        VciServiceClient::connect(format!("http://{endpoint}")),
    )
    .await
    .expect("stream gRPC connection should complete in time")
    .expect("stream gRPC client should connect");
    let mut event_stream = stream_client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(
                vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_handle),
            ),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    tokio::time::sleep(Duration::from_millis(150)).await;
    drop(stdin);

    let stream_result = timeout(Duration::from_secs(5), event_stream.message())
        .await
        .expect("subscribe_event stream should terminate within timeout");
    assert!(
        matches!(stream_result, Ok(None)),
        "subscribe_event stream should end cleanly (Ok(None)) when stdin closes and the server shuts down, got {stream_result:?}"
    );

    let exit_status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("child should exit after stdin closes even with live stream")
        .expect("child wait should succeed");
    assert!(
        exit_status.success(),
        "child exit status should be successful"
    );

    let _ = std::fs::remove_file(&config_path);
}

#[tokio::test]
#[serial]
async fn stop_terminates_live_subscribe_event_stream() {
    let config_path = write_test_library_config();
    let exe = env!("CARGO_BIN_EXE_j2534-0404-service");

    let mut child = Command::new(exe)
        .arg(format!("j2534-0404:{TEST_LIBRARY_NAME}"))
        .env(VCI_CONFIG_PATH_ENV, &config_path)
        // These tests exercise stdio/gRPC startup mechanics directly (no
        // manager involved to push an auth key via `set_auth_key`), so
        // disable ADR-221's listener auth the same way a manager-less dev
        // run would (see `vci-service-launcher::vci_server`'s doc comment
        // on this variable; this file is already `#![cfg(debug_assertions)]`
        // only, matching the variable's own debug-only compile gate).
        .env("VCI_SERVICE_INSECURE_NO_AUTH", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("j2534-0404-service child process should spawn");

    let mut stdin = child.stdin.take().expect("child stdin should be piped");
    let stdout = child.stdout.take().expect("child stdout should be piped");
    let mut stdout = BufReader::new(stdout).lines();

    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"get_status"}
"#,
        )
        .await
        .expect("status request should be written");

    let status_line = timeout(Duration::from_secs(5), stdout.next_line())
        .await
        .expect("status response should arrive in time")
        .expect("status read should succeed")
        .expect("status response line should exist");
    let status: serde_json::Value =
        serde_json::from_str(&status_line).expect("status response should be valid JSON");
    let endpoint = status["result"]["endpoints"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .filter(|s| s.parse::<std::net::SocketAddr>().is_ok())
        .expect("status response should include endpoints with a valid socket address");

    let mut setup_client = timeout(
        Duration::from_secs(5),
        VciServiceClient::connect(format!("http://{endpoint}")),
    )
    .await
    .expect("gRPC connection should complete in time")
    .expect("gRPC client should connect");
    let cll_handle = create_default_cll(&mut setup_client).await;

    let mut stream_client = timeout(
        Duration::from_secs(5),
        VciServiceClient::connect(format!("http://{endpoint}")),
    )
    .await
    .expect("stream gRPC connection should complete in time")
    .expect("stream gRPC client should connect");
    let mut event_stream = stream_client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(
                vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_handle),
            ),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    tokio::time::sleep(Duration::from_millis(150)).await;

    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":2,"method":"stop"}
"#,
        )
        .await
        .expect("stop request should be written");
    stdin.flush().await.expect("stop request should flush");

    let stop_line = timeout(Duration::from_secs(5), stdout.next_line())
        .await
        .expect("stop response should arrive in time")
        .expect("stop read should succeed")
        .expect("stop response line should exist");
    let stop_response: serde_json::Value =
        serde_json::from_str(&stop_line).expect("stop response should be valid JSON");
    assert_eq!(stop_response["result"]["stopped"], true);

    let stream_result = timeout(Duration::from_secs(5), event_stream.message())
        .await
        .expect("subscribe_event stream should terminate within timeout");
    assert!(
        matches!(stream_result, Ok(None)),
        "subscribe_event stream should end cleanly (Ok(None)) when stop shuts down the server, got {stream_result:?}"
    );

    let exit_status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("child should exit after stop response")
        .expect("child wait should succeed");
    assert!(
        exit_status.success(),
        "child exit status should be successful"
    );

    let _ = std::fs::remove_file(&config_path);
}

#[tokio::test]
#[serial]
async fn get_status_returns_startup_error_details_when_grpc_start_fails() {
    let exe = env!("CARGO_BIN_EXE_j2534-0404-service");
    let invalid_library_name = "missing-library-for-startup-error";
    // Configure a `library_path` that does not exist on disk, so the service
    // fails to load it deterministically regardless of registry state.
    let nonexistent_path = std::env::temp_dir().join(format!("{invalid_library_name}.so"));
    let config_path = unique_temp_path("config.toml");
    let config_toml = format!(
        "[config.apis.j2534-0404.libs.{invalid_library_name:?}]\nlibrary_path = {:?}\n",
        nonexistent_path.display().to_string()
    );
    std::fs::write(&config_path, config_toml).expect("test config file should be writable");

    let mut child = Command::new(exe)
        .arg(format!("j2534-0404:{invalid_library_name}"))
        .env(VCI_CONFIG_PATH_ENV, &config_path)
        // These tests exercise stdio/gRPC startup mechanics directly (no
        // manager involved to push an auth key via `set_auth_key`), so
        // disable ADR-221's listener auth the same way a manager-less dev
        // run would (see `vci-service-launcher::vci_server`'s doc comment
        // on this variable; this file is already `#![cfg(debug_assertions)]`
        // only, matching the variable's own debug-only compile gate).
        .env("VCI_SERVICE_INSECURE_NO_AUTH", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("j2534-0404-service child process should spawn");

    let mut stdin = child.stdin.take().expect("child stdin should be piped");
    let stdout = child.stdout.take().expect("child stdout should be piped");
    let mut stdout = BufReader::new(stdout).lines();

    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"get_status"}
"#,
        )
        .await
        .expect("status request should be written");

    let status_line = timeout(Duration::from_secs(5), stdout.next_line())
        .await
        .expect("status response should arrive in time")
        .expect("status read should succeed")
        .expect("status response line should exist");
    let status: serde_json::Value =
        serde_json::from_str(&status_line).expect("status response should be valid JSON");

    assert_eq!(status["result"]["library_name"], invalid_library_name);
    assert_eq!(status["result"]["running"], false);
    assert!(status["result"]["endpoints"].is_null());
    assert!(status["result"]["error"].is_string());
    // The error is intentionally sanitized: it must not echo back the
    // nonexistent library path or any raw OS/library-loading error text
    // (both could leak local filesystem details), even though this status
    // channel is local-only. Full detail is still available server-side via
    // `tracing`.
    let error_message = status["result"]["error"]
        .as_str()
        .expect("error must be a string");
    assert!(!error_message.contains(invalid_library_name));
    assert!(!error_message.contains(&nonexistent_path.display().to_string()));
    assert!(error_message.contains("failed to construct J2534-0404 API"));

    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":2,"method":"stop"}
"#,
        )
        .await
        .expect("stop request should be written");
    stdin.flush().await.expect("stop request should flush");

    let stop_line = timeout(Duration::from_secs(5), stdout.next_line())
        .await
        .expect("stop response should arrive in time")
        .expect("stop read should succeed")
        .expect("stop response line should exist");
    let stop_response: serde_json::Value =
        serde_json::from_str(&stop_line).expect("stop response should be valid JSON");
    assert_eq!(stop_response["result"]["stopped"], false);

    let exit_status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("child should exit after stop response")
        .expect("child wait should succeed");
    assert!(
        exit_status.success(),
        "child exit status should be successful"
    );

    let _ = std::fs::remove_file(&config_path);
}

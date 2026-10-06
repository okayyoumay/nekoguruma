// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! The agent-side path to a worker: `worker-host` launches the real
//! `j2534-0404-service` binary against the mock library, provisions its auth
//! key, and connects a gRPC client that mints bearer tokens from that key
//! (ADR-221). Calls without a token, or with a token signed by another key,
//! are rejected by the listener.
//!
//! This file holds a single test, so the process-wide `VCI_CONFIG_PATH` it
//! sets for the spawned service cannot race with another test.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use j2534_0404_mock::mock_library_path;
use vci_service_interface::GetModuleIdsRequest;
use vci_service_interface::vci_service_client::VciServiceClient;
use worker_host::client::{ConnectOptions, connect};
use worker_host::service::{LaunchOptions, ServiceKind, WorkerProcess};

const TEST_LIBRARY_NAME: &str = "mock-worker-host-library";

#[tokio::test]
async fn worker_host_client_authenticates_with_minted_tokens() {
    let mock_library =
        mock_library_path().expect("mock library should be discoverable after build");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let config_path =
        std::env::temp_dir().join(format!("j2534-0404-service-{nanos}-worker-host.toml"));
    std::fs::write(
        &config_path,
        format!(
            "[config.apis.j2534-0404.libs.{TEST_LIBRARY_NAME:?}]\nlibrary_path = {:?}\n",
            mock_library.display().to_string()
        ),
    )
    .expect("test config file should be writable");
    // SAFETY: this test binary runs only this test, so no other thread reads
    // or writes the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
    }

    let worker = WorkerProcess::launch(
        std::path::Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        ServiceKind::J2534V0404,
        TEST_LIBRARY_NAME,
        // Generous timeouts for a debug binary on a busy CI runner.
        &LaunchOptions {
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(5),
            ..LaunchOptions::default()
        },
    )
    .await
    .expect("worker should launch");

    let mut client = worker
        .connect(&ConnectOptions::default())
        .await
        .expect("client should connect");
    let modules = client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect("an authenticated call should succeed")
        .into_inner();
    assert!(modules.module_id_list.is_some());

    // Every reported endpoint (IPv4 and, where the host has it, IPv6 loopback) accepts an
    // authenticated client on its own.
    for endpoint in worker.endpoints() {
        let mut client = connect(
            std::slice::from_ref(endpoint),
            *worker.auth_key(),
            &ConnectOptions::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("client should connect to {endpoint}: {e}"));
        client
            .get_module_ids(GetModuleIdsRequest {})
            .await
            .unwrap_or_else(|e| panic!("an authenticated call to {endpoint} should succeed: {e}"));
    }

    let mut anonymous = VciServiceClient::connect(format!("http://{}", worker.endpoints()[0]))
        .await
        .expect("plain client should connect");
    let status = anonymous
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect_err("a call without a token should be rejected");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);

    let mut wrong_key = connect(worker.endpoints(), [0u8; 32], &ConnectOptions::default())
        .await
        .expect("client should connect");
    let status = wrong_key
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect_err("a token signed with another key should be rejected");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
    let _ = std::fs::remove_file(&config_path);
}

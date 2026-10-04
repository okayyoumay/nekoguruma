//! Lifecycle tests against the `fake-vci-service` test double.

use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use worker_host::service::{LaunchOptions, ServiceError, ServiceKind, WorkerProcess};

fn fake() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_fake-vci-service"))
}

async fn launch(library: &str, startup_timeout: Duration) -> Result<WorkerProcess, ServiceError> {
    let options = LaunchOptions {
        startup_timeout,
        ..LaunchOptions::default()
    };
    WorkerProcess::launch(fake(), ServiceKind::J2534V0404, library, &options).await
}

#[tokio::test]
async fn launch_provisions_key_and_reports_endpoints() {
    let mut worker = launch("Acme - Turtle", Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(worker.endpoints(), ["127.0.0.1:50051", "[::1]:50051"]);

    let echoed = worker.request("echo_auth_key", None).await.unwrap();
    let expected = base64::engine::general_purpose::STANDARD.encode(worker.auth_key());
    assert_eq!(echoed["key"], expected);
    assert_eq!(echoed["library"], "Acme - Turtle");

    assert!(matches!(
        worker.request("no_such_method", None).await,
        Err(ServiceError::Rpc { code: -32601, .. })
    ));
    worker.stop(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn reports_startup_error() {
    let result = launch("startup-error", Duration::from_secs(5)).await;
    assert!(matches!(result, Err(ServiceError::StartupFailed(m)) if m == "library not found"));
}

#[tokio::test]
async fn times_out_when_never_running() {
    let result = launch("never-running", Duration::from_millis(500)).await;
    assert!(matches!(result, Err(ServiceError::Timeout(_))));
}

#[tokio::test]
async fn rejects_query_in_library_name() {
    let result = launch("a?port=1", Duration::from_secs(5)).await;
    assert!(matches!(result, Err(ServiceError::InvalidLibraryName(_))));
}

#[tokio::test]
async fn missing_binary_is_a_spawn_error() {
    let result = WorkerProcess::launch(
        Path::new("/nonexistent/worker"),
        ServiceKind::Iso22900,
        "X",
        &LaunchOptions::default(),
    )
    .await;
    assert!(matches!(result, Err(ServiceError::Spawn(_))));
}

#[tokio::test]
async fn passes_long_size_to_the_service() {
    for (long_size, expected) in [(None, serde_json::Value::Null), (Some(8), "8".into())] {
        let options = LaunchOptions {
            long_size,
            ..LaunchOptions::default()
        };
        let mut worker = WorkerProcess::launch(fake(), ServiceKind::J2534V0404, "lib", &options)
            .await
            .unwrap();
        let echoed = worker.request("echo_long_size", None).await.unwrap();
        assert_eq!(echoed["long_size"], expected);
        worker.stop(Duration::from_secs(2)).await.unwrap();
    }
    let options = LaunchOptions {
        long_size: Some(2),
        ..LaunchOptions::default()
    };
    assert!(matches!(
        WorkerProcess::launch(fake(), ServiceKind::J2534V0404, "lib", &options).await,
        Err(ServiceError::InvalidLongSize(2))
    ));
}

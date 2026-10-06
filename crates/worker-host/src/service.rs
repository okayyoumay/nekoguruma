//! Launching and controlling a worker process (`j2534-0404-service` / `iso22900-service`).
//!
//! Contract of the service binaries (`crates/j2534-0404-service/docs/startup-spec.md`):
//!
//! - First argument `<scheme>:<library name>[?port=<port>]`. The service resolves the library
//!   name itself (registry on Windows, its `config.toml` elsewhere), within its own registry view.
//! - Control channel: one JSON-RPC 2.0 document per line on stdin / stdout.
//!   `set_auth_key` installs the per-instance HMAC key that the gRPC listener verifies bearer
//!   tokens with; `get_status` reports the loopback gRPC endpoints; `stop` shuts down.
//! - Closing stdin also shuts the service down.
//!
//! The auth key is generated here and is held only by the agent and the worker it is provisioned
//! to; it never travels over the gRPC socket (ADR-221, ADR-231).
//!
//! The width of J2534 `unsigned long` is passed to the j2534-0404 service through
//! [`LONG_SIZE_ENV`]; its sys layer converts at the FFI boundary (`docs/worker-crates.md`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::abi::Abi;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceKind {
    J2534V0404,
    Iso22900,
}

impl ServiceKind {
    pub fn binary_name(self) -> &'static str {
        match self {
            ServiceKind::J2534V0404 => "j2534-0404-service",
            ServiceKind::Iso22900 => "iso22900-service",
        }
    }

    pub fn startup_scheme(self) -> &'static str {
        match self {
            ServiceKind::J2534V0404 => "j2534-0404",
            ServiceKind::Iso22900 => "iso22900",
        }
    }
}

/// Installed workers: `<root>/<abi name>/<binary name>[.exe]`, one directory per ABI (7.3).
#[derive(Debug, Clone)]
pub struct WorkerLayout {
    pub root: PathBuf,
}

impl WorkerLayout {
    pub fn binary_path(&self, kind: ServiceKind, abi: Abi) -> PathBuf {
        let suffix = match abi {
            Abi::WinX86 | Abi::WinX64 => ".exe",
            _ => "",
        };
        self.root
            .join(abi.name())
            .join(format!("{}{suffix}", kind.binary_name()))
    }

    /// The worker binary for `abi`, or [`ServiceError::NoWorkerForAbi`] if that build is not
    /// bundled (reported as `UNSUPPORTED_ABI`, 7.3).
    pub fn find(&self, kind: ServiceKind, abi: Abi) -> Result<PathBuf, ServiceError> {
        let path = self.binary_path(kind, abi);
        if path.is_file() {
            Ok(path)
        } else {
            Err(ServiceError::NoWorkerForAbi { abi, path })
        }
    }
}

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Upper bound for `set_auth_key` + `get_status` polling until the gRPC server runs.
    pub startup_timeout: Duration,
    /// Timeout for each control request.
    pub request_timeout: Duration,
    pub poll_interval: Duration,
    /// Width of J2534 `unsigned long` in bytes (4 or 8): the registration definition's
    /// `long_size`, else [`Abi::default_long_size`] (7.1.2). `None` leaves the service default
    /// (4). Only the j2534-0404 service reads it.
    pub long_size: Option<u8>,
}

/// Environment variable read by the j2534-0404 service (`j2534_0404_sys::LONG_SIZE_ENV`).
pub const LONG_SIZE_ENV: &str = "NGR_J2534_LONG_SIZE";

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(2),
            poll_interval: Duration::from_millis(50),
            long_size: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("no worker bundled for ABI {}: {}", abi.name(), path.display())]
    NoWorkerForAbi { abi: Abi, path: PathBuf },
    #[error("invalid long_size {0} (must be 4 or 8)")]
    InvalidLongSize(u8),
    #[error("invalid library name {0:?}")]
    InvalidLibraryName(String),
    #[error("failed to launch worker: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("worker control channel I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("worker exited")]
    Exited,
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("malformed control response: {0}")]
    Protocol(String),
    #[error("worker returned error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("worker failed to start: {0}")]
    StartupFailed(String),
    #[error("cannot generate auth key: {0}")]
    Random(String),
}

/// A running worker process. Dropping it kills the process.
pub struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    auth_key: [u8; 32],
    endpoints: Vec<String>,
    request_timeout: Duration,
}

impl WorkerProcess {
    /// Launches `binary` for `library_name`, provisions a fresh auth key and waits until the
    /// gRPC server reports its endpoints.
    pub async fn launch(
        binary: &Path,
        kind: ServiceKind,
        library_name: &str,
        options: &LaunchOptions,
    ) -> Result<Self, ServiceError> {
        if library_name.trim().is_empty() || library_name.contains('?') {
            return Err(ServiceError::InvalidLibraryName(library_name.to_owned()));
        }
        let mut command = Command::new(binary);
        command.env_remove(LONG_SIZE_ENV);
        if let Some(size) = options.long_size {
            if size != 4 && size != 8 {
                return Err(ServiceError::InvalidLongSize(size));
            }
            command.env(LONG_SIZE_ENV, size.to_string());
        }
        let mut child = command
            .arg(format!("{}:{library_name}", kind.startup_scheme()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(ServiceError::Spawn)?;
        let stdin = child.stdin.take().ok_or(ServiceError::Exited)?;
        let stdout = BufReader::new(child.stdout.take().ok_or(ServiceError::Exited)?).lines();

        let mut auth_key = [0u8; 32];
        getrandom::fill(&mut auth_key).map_err(|e| ServiceError::Random(e.to_string()))?;

        let mut worker = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            auth_key,
            endpoints: Vec::new(),
            request_timeout: options.request_timeout,
        };

        let startup = async {
            let key = base64::engine::general_purpose::STANDARD.encode(worker.auth_key);
            worker
                .request("set_auth_key", Some(json!({ "key": key })))
                .await?;
            loop {
                let status = worker.request("get_status", None).await?;
                if let Some(error) = status.get("error").and_then(Value::as_str) {
                    return Err(ServiceError::StartupFailed(error.to_owned()));
                }
                if status.get("running").and_then(Value::as_bool) == Some(true) {
                    let endpoints = parse_endpoints(&status)?;
                    worker.endpoints = endpoints;
                    return Ok(());
                }
                tokio::time::sleep(options.poll_interval).await;
            }
        };
        tokio::time::timeout(options.startup_timeout, startup)
            .await
            .map_err(|_| ServiceError::Timeout("worker startup"))??;
        Ok(worker)
    }

    /// Loopback gRPC endpoints (e.g. `127.0.0.1:54321`, `[::1]:54321`). Pick a reachable one.
    pub fn endpoints(&self) -> &[String] {
        &self.endpoints
    }

    /// Per-instance key for minting bearer tokens for the gRPC listener.
    pub fn auth_key(&self) -> &[u8; 32] {
        &self.auth_key
    }

    /// Sends a JSON-RPC request and returns its `result`.
    pub async fn request(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, ServiceError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(params) = params {
            msg["params"] = params;
        }
        let mut line = msg.to_string();
        line.push('\n');

        let exchange = async {
            self.stdin.write_all(line.as_bytes()).await?;
            self.stdin.flush().await?;
            loop {
                let line = self.stdout.next_line().await?.ok_or(ServiceError::Exited)?;
                // Ignore anything that is not the response to this request.
                let Ok(response) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if response.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                return parse_response(response);
            }
        };
        tokio::time::timeout(self.request_timeout, exchange)
            .await
            .map_err(|_| ServiceError::Timeout("control response"))?
    }

    /// Asks the worker to stop and waits for it to exit; kills it if it does not.
    pub async fn stop(mut self, grace: Duration) -> Result<(), ServiceError> {
        let requested = self.request("stop", None).await;
        match tokio::time::timeout(grace, self.child.wait()).await {
            Ok(status) => {
                status?;
            }
            Err(_) => {
                self.child.kill().await?;
            }
        }
        match requested {
            Ok(_) | Err(ServiceError::Exited) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

fn parse_response(response: Value) -> Result<Value, ServiceError> {
    if let Some(error) = response.get("error") {
        return Err(ServiceError::Rpc {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        });
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| ServiceError::Protocol("response has neither result nor error".into()))
}

fn parse_endpoints(status: &Value) -> Result<Vec<String>, ServiceError> {
    let endpoints: Vec<String> = status
        .get("endpoints")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if endpoints.is_empty() {
        return Err(ServiceError::Protocol("running without endpoints".into()));
    }
    Ok(endpoints)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_paths() {
        let layout = WorkerLayout {
            root: PathBuf::from("workers"),
        };
        assert_eq!(
            layout.binary_path(ServiceKind::J2534V0404, Abi::WinX86),
            Path::new("workers")
                .join("win-x86")
                .join("j2534-0404-service.exe")
        );
        assert_eq!(
            layout.binary_path(ServiceKind::Iso22900, Abi::LinuxArm64),
            Path::new("workers")
                .join("linux-arm64")
                .join("iso22900-service")
        );
        assert!(matches!(
            layout.find(ServiceKind::Iso22900, Abi::LinuxArm64),
            Err(ServiceError::NoWorkerForAbi { .. })
        ));
    }

    #[test]
    fn response_parsing() {
        assert_eq!(
            parse_response(json!({"id": 1, "result": {"a": 1}})).unwrap(),
            json!({"a": 1})
        );
        assert!(matches!(
            parse_response(json!({"id": 1, "error": {"code": -32601, "message": "nope"}})),
            Err(ServiceError::Rpc { code: -32601, .. })
        ));
        assert!(parse_endpoints(&json!({"running": true, "endpoints": null})).is_err());
    }
}

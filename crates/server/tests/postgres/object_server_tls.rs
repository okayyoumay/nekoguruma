//! The pinned object server (RustFS) speaks TLS under a self-signed P-256 certificate (ADR-505):
//! it serves its readiness route to a client that trusts only that certificate, and a plain
//! HTTP request to the same port gets no HTTP answer. The child is started here with the
//! certificate in the directory `RUSTFS_TLS_PATH` names; the server's own start does not do so
//! yet. Like the other real-binary tests it needs `NGR_TEST_RUSTFS_BIN`.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::object_server_rustfs::rustfs_bin;
use crate::support::{free_port, random_password};

/// How long the child may take to answer over TLS.
const READY_WITHIN: Duration = Duration::from_secs(60);

/// The child, killed when the test ends however it ends.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A self-signed P-256 certificate for `127.0.0.1`, written as RustFS reads it: the
/// certificate and the key in PEM, under the names it looks for in `RUSTFS_TLS_PATH`.
fn write_certificate(dir: &Path) -> CertificateDer<'static> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let params = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    let certificate = params.self_signed(&key).unwrap();
    std::fs::write(dir.join("rustfs_cert.pem"), certificate.pem()).unwrap();
    std::fs::write(dir.join("rustfs_key.pem"), key.serialize_pem()).unwrap();
    certificate.der().clone()
}

fn start(dir: &Path, tls: &Path, port: u16) -> Running {
    let data = dir.join("data");
    std::fs::create_dir(&data).unwrap();
    let mut command = Command::new(rustfs_bin());
    command
        .arg("server")
        .env_clear()
        .env("RUSTFS_ADDRESS", format!("127.0.0.1:{port}"))
        .env("RUSTFS_VOLUMES", &data)
        .env("RUSTFS_CONSOLE_ENABLE", "false")
        .env("RUSTFS_ACCESS_KEY", random_password())
        .env("RUSTFS_SECRET_KEY", random_password())
        .env("RUSTFS_TLS_PATH", tls)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.env("HOME", dir).env("PATH", "/usr/bin:/bin");
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    Running(command.spawn().unwrap())
}

/// The status of `GET <path>` over TLS to `addr`, trusting `certificate` alone.
async fn tls_get(
    addr: SocketAddr,
    certificate: &CertificateDer<'static>,
    path: &str,
) -> std::io::Result<u16> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(certificate.clone())
        .map_err(std::io::Error::other)?;
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(std::io::Error::other)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(addr).await?;
    let name = ServerName::IpAddress(Ipv4Addr::LOCALHOST.into());
    let mut stream = connector.connect(name, tcp).await?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await?;
    let mut head = Vec::new();
    stream.read_to_end(&mut head).await.or_else(|error| {
        // A peer that closes without a TLS close_notify after a complete answer.
        if error.kind() == std::io::ErrorKind::UnexpectedEof && !head.is_empty() {
            Ok(0)
        } else {
            Err(error)
        }
    })?;
    let text = String::from_utf8_lossy(&head);
    text.split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("no status line in {text:?}")))
}

/// What a plain HTTP request to `addr` gets back, up to 64 bytes, within five seconds.
async fn plain_answer(addr: SocketAddr) -> Vec<u8> {
    let attempt = async {
        let mut stream = TcpStream::connect(addr).await?;
        stream
            .write_all(
                format!("GET /health/ready HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await?;
        let mut answer = vec![0u8; 64];
        let n = stream.read(&mut answer).await?;
        answer.truncate(n);
        Ok::<Vec<u8>, std::io::Error>(answer)
    };
    match tokio::time::timeout(Duration::from_secs(5), attempt).await {
        Ok(Ok(answer)) => answer,
        _ => Vec::new(),
    }
}

/// ADR-505: the child serves its readiness route over TLS to a client that trusts only the
/// certificate it was given, and a plain HTTP request gets no HTTP answer.
#[tokio::test]
async fn the_real_server_serves_over_tls_and_refuses_plain_http() {
    let dir = tempfile::tempdir().unwrap();
    let tls = dir.path().join("tls");
    std::fs::create_dir(&tls).unwrap();
    let certificate = write_certificate(&tls);
    let port = free_port();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut child = start(dir.path(), &tls, port);

    let began = Instant::now();
    let ready = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            panic!("the object server ended with {status} before it answered over TLS");
        }
        match tls_get(addr, &certificate, "/health/ready").await {
            Ok(status) => break status,
            Err(_) if began.elapsed() < READY_WITHIN => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(error) => panic!("no TLS answer within {READY_WITHIN:?}: {error}"),
        }
    };
    assert_eq!(ready, 200, "the readiness route over TLS");

    let plain = plain_answer(addr).await;
    assert!(
        !plain.starts_with(b"HTTP/"),
        "a plain HTTP request got an HTTP answer: {:?}",
        String::from_utf8_lossy(&plain)
    );
    drop(child);
}

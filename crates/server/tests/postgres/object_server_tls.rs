//! The pinned object server (RustFS) speaks TLS only, under the certificate the server made, and
//! the server's own start and store client reach it with the pin (ADR-505): a client that trusts
//! the pin gets the readiness route, plain HTTP to the same port gets no HTTP answer, and a
//! process that shares the port receives nothing but handshake records. Like the other
//! real-binary tests these need `NGR_TEST_RUSTFS_BIN`.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::object_server_rustfs::Real;
use crate::support::pinned_get;

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

/// ADR-505: the object server the server started serves its readiness route over TLS to a client
/// that trusts only the server's pin, refuses a client that trusts another, and a plain HTTP
/// request gets no HTTP answer.
#[tokio::test]
async fn the_real_server_serves_over_tls_and_refuses_plain_http() {
    let mut real = Real::new();
    let server = real.start().await;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, server.port()));

    assert_eq!(
        pinned_get(addr, server.certificate_pin(), "/health/ready")
            .await
            .unwrap(),
        200,
        "the readiness route over TLS"
    );
    // A pin of another certificate is refused: no status comes back.
    let other =
        ngr_pinned_tls::test_server::Identity::generate(ngr_pinned_tls::test_server::Key::P256);
    let other_pin = ngr_pinned_tls::CertPin::of_certificate(other.certificate());
    assert!(
        pinned_get(addr, &other_pin, "/health/ready").await.is_err(),
        "a client that pins another certificate must refuse the server"
    );

    let plain = plain_answer(addr).await;
    assert!(
        !plain.starts_with(b"HTTP/"),
        "a plain HTTP request got an HTTP answer: {:?}",
        String::from_utf8_lossy(&plain)
    );
    server.stop(std::future::pending()).await;
}

/// ADR-505, ADR-504 item 5: a process of the same account that shares the object server's port
/// (Linux lets one with `SO_REUSEPORT` bind it) is given about half of the store's connections,
/// and it receives nothing but a handshake record: no request line, no signature, no part.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_process_sharing_the_port_receives_only_handshake_records_during_store_operations() {
    use std::sync::{Arc, Mutex};

    use server::storage::{
        MultipartStore, ObjectKey, S3Store, S3Tuning, StoreError, Upload, UploadId,
    };

    const MARKER: &str = "PART-MARKER-4f2a9c";
    let mut real = Real::new();
    let server = real.start().await;
    let port = server.port();

    // The sharer: bound to the same port with SO_REUSEPORT, it reads each connection's first
    // bytes, keeps them, and closes.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_reuseport(true).unwrap();
    socket
        .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .expect("the same account can share the port with SO_REUSEPORT");
    let listener = socket.listen(64).unwrap();
    let heard: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let keeper = Arc::clone(&heard);
    let sharer = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = vec![0u8; 8192];
            let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);
            buf.truncate(n);
            keeper.lock().unwrap().push(buf);
        }
    });

    let store = tokio::time::timeout(
        Duration::from_secs(60),
        S3Store::open(
            port,
            server.credentials(),
            server.certificate_pin(),
            "nekoguruma",
            S3Tuning::default(),
        ),
    )
    .await
    .expect("open must not hang")
    .unwrap_or_else(|error| panic!("the store must open despite a sharer: {error}"));

    let (mut reached, mut caught) = (0, 0);
    for _ in 0..40 {
        let upload = Upload {
            key: ObjectKey::parse("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d").unwrap(),
            id: UploadId::parse("an-upload-id").unwrap(),
        };
        match store.put_part(&upload, 1, MARKER.as_bytes().to_vec()).await {
            Err(StoreError::Untrusted { .. }) => caught += 1,
            // The real object server answers; this upload does not exist there.
            Err(_) | Ok(()) => reached += 1,
        }
    }
    sharer.abort();

    let heard = std::mem::take(&mut *heard.lock().unwrap());
    assert!(
        !heard.is_empty(),
        "the sharer got no connection in 40 operations"
    );
    assert!(caught >= 1, "no operation met the sharer");
    assert!(reached >= 1, "no operation reached the object server");
    for bytes in heard.iter() {
        // A TLS record of type handshake (0x16) is all a client sends before it has accepted
        // the server.
        if let Some(first) = bytes.first() {
            assert_eq!(*first, 0x16, "the first record is not a handshake record");
        }
        let text = String::from_utf8_lossy(bytes);
        for word in [MARKER, "PUT ", "GET ", "HTTP/1.1", "X-Amz", "nekoguruma"] {
            assert!(!text.contains(word), "the sharer heard {word:?}");
        }
    }
    server.stop(std::future::pending()).await;
}

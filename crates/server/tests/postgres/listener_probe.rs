//! The probe that a listening address is shared with nobody (ADR-504 item 5): while the owner
//! listens on `127.0.0.1:<port>`, no other socket can take that address, so no other process can
//! receive a connection meant for the owner. It runs against the server's own listener
//! (`serving.rs`) and against the pinned object server's (`object_server_rustfs.rs`).

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

/// How many connections must reach the owner while a wildcard socket shares its port.
const CONNECTIONS: usize = 16;

/// Checks the owner's listening address `addr`:
/// - a second bind of `addr` with address reuse fails, and on Unix one with port reuse;
/// - a wildcard bind of the same port fails on Linux, with and without address reuse;
/// - where a wildcard bind succeeds (Windows may allow it), every connection to `addr` is still
///   answered by the owner, not left with the wildcard socket.
///
/// The owner must answer an HTTP `GET /` with a status line; its status does not matter.
pub(crate) async fn assert_unshared(addr: SocketAddr) {
    assert_eq!(
        addr.ip(),
        std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
        "the owner listens on {addr}, not on 127.0.0.1"
    );
    assert!(
        owner_answers(addr).await,
        "the owner at {addr} does not answer before the probe"
    );

    let second = TcpSocket::new_v4().unwrap();
    second.set_reuseaddr(true).unwrap();
    assert!(
        second.bind(addr).is_err(),
        "a bind with address reuse took {addr}"
    );
    #[cfg(unix)]
    {
        let second = TcpSocket::new_v4().unwrap();
        second.set_reuseport(true).unwrap();
        assert!(
            second.bind(addr).is_err(),
            "a bind with port reuse took {addr}"
        );
    }

    for reuse in [false, true] {
        let wildcard = TcpSocket::new_v4().unwrap();
        wildcard.set_reuseaddr(reuse).unwrap();
        let bound = wildcard.bind((Ipv4Addr::UNSPECIFIED, addr.port()).into());
        if cfg!(target_os = "linux") {
            assert!(
                bound.is_err(),
                "a wildcard bind took port {} (address reuse {reuse})",
                addr.port()
            );
            continue;
        }
        if bound.is_err() {
            eprintln!("wildcard bind refused (address reuse {reuse})");
            continue;
        }
        eprintln!("wildcard bind taken (address reuse {reuse}): checking the connections");
        // The wildcard socket listens but never accepts: a connection it got would never be
        // answered, and `owner_answers` would see that.
        let _wildcard = wildcard.listen(CONNECTIONS as u32).unwrap();
        for i in 0..CONNECTIONS {
            assert!(
                owner_answers(addr).await,
                "connection {i} to {addr} was not answered by the owner (address reuse {reuse})"
            );
        }
    }
}

/// Whether a `GET /` to `addr` gets an HTTP status line back within the bound.
async fn owner_answers(addr: SocketAddr) -> bool {
    let attempt = async {
        let mut stream = TcpStream::connect(addr).await?;
        stream
            .write_all(
                format!("GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
            )
            .await?;
        let mut head = [0u8; 12];
        stream.read_exact(&mut head).await?;
        Ok::<bool, std::io::Error>(head.starts_with(b"HTTP/1."))
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(5), attempt).await,
        Ok(Ok(true))
    )
}

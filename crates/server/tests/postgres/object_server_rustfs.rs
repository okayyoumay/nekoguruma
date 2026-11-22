//! The real pinned object server (RustFS) as the server's child (ADR-491): readiness, the stop,
//! the loopback-only listener and the credentials, against the binary that
//! `scripts/fetch-rustfs.sh` fetches. The tests need that binary and fail, naming the script, when
//! `NGR_TEST_RUSTFS_BIN` does not point at it (as a missing PostgreSQL fails the database tests,
//! ADR-458); CI exports the variable from the script's output.
//!
//! The fake child's tests (`object_server.rs`) cover the lifecycle's edge cases; these tests are
//! the ones that need what only the real program does.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use server::config::ObjectServerConfig;
use server::object_server::credentials::{ACCESS_KEY_FILE, SECRET_KEY_FILE};
use server::object_server::{ObjectServer, Tuning};

use crate::redeem::capture;
use crate::support::free_port;

/// The pinned binary, or a panic that names the script that fetches it.
pub(crate) fn rustfs_bin() -> PathBuf {
    let Some(path) = std::env::var_os("NGR_TEST_RUSTFS_BIN") else {
        panic!(
            "NGR_TEST_RUSTFS_BIN is not set: the server's tests start the pinned RustFS; run \
             scripts/fetch-rustfs.sh and set the variable to the path it prints"
        );
    };
    let path = PathBuf::from(path);
    assert!(
        path.is_file(),
        "NGR_TEST_RUSTFS_BIN is {} but that is no file; run scripts/fetch-rustfs.sh",
        path.display()
    );
    path
}

pub(crate) struct Real {
    _dir: tempfile::TempDir,
    pub(crate) config: ObjectServerConfig,
}

impl Real {
    pub(crate) fn new() -> Real {
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("credentials");
        std::fs::create_dir(&credentials).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Real {
            config: ObjectServerConfig {
                binary: rustfs_bin(),
                data_dir: dir.path().join("data"),
                credentials_dir: credentials,
                port: Some(free_port()),
                bucket: "nekoguruma".to_owned(),
            },
            _dir: dir,
        }
    }

    fn port(&self) -> u16 {
        self.config.port.unwrap()
    }

    /// Starts the real child on a port that is free at that moment, with a new port when another
    /// process takes it (before the spawn, or while the child binds it: RustFS then ends with
    /// status 1, which the start's error does not explain, so `object_port_lost` looks whether
    /// something answers on the port). The port that was used is kept in `config`.
    pub(crate) async fn start(&mut self) -> ObjectServer {
        let (result, port) = tokio::time::timeout(
            Duration::from_secs(60 * crate::support::PORT_ATTEMPTS as u64),
            crate::support::start_object_server(&self.config, &Tuning::default()),
        )
        .await
        .expect("the start must not hang");
        self.config.port = Some(port);
        result.unwrap_or_else(|error| panic!("the real object server must start: {error}"))
    }
}

/// The status code of `GET <path>` to `addr`, or the error of the connection.
fn get(addr: SocketAddr, path: &str) -> std::io::Result<u16> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )?;
    let mut text = String::new();
    let mut buf = [0u8; 512];
    while !text.contains("\r\n") {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        text.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    text.split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("no status line in {text:?}")))
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Q1, L5: the real binary is ready within the bound, answers the readiness probe, and an
/// anonymous request is refused (403).
#[tokio::test]
async fn the_real_server_is_ready_and_refuses_an_anonymous_request() {
    let (log, _guard) = capture();
    let mut real = Real::new();
    let began = Instant::now();
    let server = real.start().await;
    assert!(began.elapsed() < Duration::from_secs(30));
    let port = real.port();
    let (ready, anonymous, bucket) = tokio::task::spawn_blocking(move || {
        (
            get(loopback(port), "/health/ready").unwrap(),
            get(loopback(port), "/").unwrap(),
            get(loopback(port), "/some-bucket/some-key").unwrap(),
        )
    })
    .await
    .unwrap();
    assert_eq!(ready, 200);
    assert_eq!(anonymous, 403, "an anonymous request must be refused");
    assert!(bucket == 403 || bucket == 404, "{bucket}");
    // The secret is in no line the child wrote.
    let secret = server.credentials().expose_secret().to_owned();
    // T1: the stop ends it within the bound.
    let stopped = Instant::now();
    server.stop(std::future::pending()).await;
    assert!(
        stopped.elapsed() < Duration::from_secs(10),
        "{:?}",
        stopped.elapsed()
    );
    assert!(!log.text().contains(&secret), "the secret is in the log");
    assert!(
        tokio::task::spawn_blocking(move || get(loopback(port), "/health/ready").is_err())
            .await
            .unwrap(),
        "the listener must be closed after the stop"
    );
}

/// The host's non-loopback IPv4 address, or `None` when it has none.
fn non_loopback_ipv4() -> Option<Ipv4Addr> {
    // The address the system picks as a source towards a routable one; nothing is sent.
    if let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        && socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).is_ok()
        && let Ok(SocketAddr::V4(local)) = socket.local_addr()
        && !local.ip().is_loopback()
        && !local.ip().is_unspecified()
    {
        return Some(*local.ip());
    }
    // Linux without a default route: the local addresses of the routing table.
    let trie = std::fs::read_to_string("/proc/net/fib_trie").ok()?;
    let mut last = None;
    for line in trie.lines() {
        let line = line.trim();
        if let Some(addr) = line.strip_prefix("|-- ") {
            last = addr.parse::<Ipv4Addr>().ok();
        } else if line.contains("/32 host LOCAL")
            && let Some(addr) = last
            && !addr.is_loopback()
        {
            return Some(addr);
        }
    }
    None
}

/// L2, L3: the listener is on 127.0.0.1 only: the host's other IPv4 address and `[::1]` refuse,
/// while a listener of the test's own on `0.0.0.0` is reachable at that address (the control that
/// shows the address works at all). A host with no such address fails the test under CI and skips
/// it elsewhere.
#[tokio::test]
async fn the_real_server_refuses_connections_that_are_not_loopback() {
    let mut real = Real::new();
    let server = real.start().await;
    let port = real.port();
    let Some(host) = non_loopback_ipv4() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "this host has no non-loopback IPv4 address, so L2 cannot be checked"
        );
        eprintln!("skipped L2: no non-loopback IPv4 address on this host");
        server.stop(std::future::pending()).await;
        return;
    };
    let control = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    let control_port = control.local_addr().unwrap().port();
    let (control_ok, refused, ipv6_refused, loopback_ok) = tokio::task::spawn_blocking(move || {
        let reachable = TcpStream::connect_timeout(
            &SocketAddr::new(IpAddr::V4(host), control_port),
            Duration::from_secs(5),
        )
        .is_ok();
        let refused = TcpStream::connect_timeout(
            &SocketAddr::new(IpAddr::V4(host), port),
            Duration::from_secs(5),
        )
        .is_err();
        let ipv6_refused = TcpStream::connect_timeout(
            &SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), port),
            Duration::from_secs(5),
        )
        .is_err();
        (
            reachable,
            refused,
            ipv6_refused,
            get(loopback(port), "/health/ready").is_ok(),
        )
    })
    .await
    .unwrap();
    assert!(
        control_ok,
        "the control listener must be reachable at {host}"
    );
    assert!(
        refused,
        "the object server accepted a connection at {host}:{port}"
    );
    assert!(
        ipv6_refused,
        "the object server accepted a connection at [::1]:{port}"
    );
    assert!(loopback_ok);
    server.stop(std::future::pending()).await;
}

/// The inodes of the sockets a process holds (Linux).
#[cfg(target_os = "linux")]
fn socket_inodes(pid: u32) -> Vec<String> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .filter_map(|t| {
            let t = t.to_string_lossy().into_owned();
            t.strip_prefix("socket:[")
                .and_then(|s| s.strip_suffix(']'))
                .map(str::to_owned)
        })
        .collect()
}

/// L4: the child listens on one socket, 127.0.0.1:<port> (no console, no metrics port, no IPv6).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_real_server_has_no_other_listener() {
    let mut real = Real::new();
    let server = real.start().await;
    let inodes = socket_inodes(server.pid().unwrap());
    let mut listening = Vec::new();
    for table in ["tcp", "tcp6"] {
        // A host without IPv6 has no tcp6 table.
        let text = std::fs::read_to_string(format!("/proc/net/{table}")).unwrap_or_default();
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // local address, state 0A (LISTEN), inode.
            if fields.len() > 9 && fields[3] == "0A" && inodes.iter().any(|i| i == fields[9]) {
                listening.push(format!("{table} {}", fields[1]));
            }
        }
    }
    let want = format!("tcp 0100007F:{:04X}", real.port());
    assert_eq!(listening, [want], "the child's listening sockets");
    server.stop(std::future::pending()).await;
}

/// ADR-504 item 5 for the object server: while the real child runs, no other socket can take
/// its address, so no other process can receive a presigned part's bytes or serve a download in
/// its place. Its listener options are RustFS's own, so this runs on every platform the tests do.
#[tokio::test]
async fn the_real_servers_listener_shares_its_address_with_nobody() {
    let mut real = Real::new();
    let server = real.start().await;
    crate::listener_probe::assert_unshared(loopback(real.port())).await;
    server.stop(std::future::pending()).await;
}

/// L6, R11: the secret is in no argument and no environment of the real child.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_real_servers_command_line_and_environment_hold_no_secret() {
    let mut real = Real::new();
    let server = real.start().await;
    let pid = server.pid().unwrap();
    let secret = server.credentials().expose_secret().to_owned();
    for name in ["cmdline", "environ"] {
        let bytes = std::fs::read(format!("/proc/{pid}/{name}")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(&secret), "{name}");
    }
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    let keys: Vec<String> = environ
        .split(|&b| b == 0)
        .filter_map(|e| {
            String::from_utf8_lossy(e)
                .split('=')
                .next()
                .map(str::to_owned)
        })
        .filter(|k| !k.is_empty())
        .collect();
    assert!(
        keys.iter()
            .all(|k| k == "HOME" || k == "PATH" || k.starts_with("RUSTFS_")),
        "{keys:?}"
    );
    server.stop(std::future::pending()).await;
}

/// K10: credential files deleted while the data directory is kept: new ones are made and used,
/// and RustFS 1.0.6 starts with them over the old data.
#[tokio::test]
async fn deleted_credential_files_are_made_again_and_used() {
    let mut real = Real::new();
    let first = real.start().await;
    let old = first.credentials().expose_secret().to_owned();
    first.stop(std::future::pending()).await;
    for name in [ACCESS_KEY_FILE, SECRET_KEY_FILE] {
        std::fs::remove_file(real.config.credentials_dir.join(name)).unwrap();
    }
    let second = real.start().await;
    let new = std::fs::read_to_string(real.config.credentials_dir.join(SECRET_KEY_FILE)).unwrap();
    assert_ne!(new, old);
    assert_eq!(second.credentials().expose_secret(), new);
    second.stop(std::future::pending()).await;
}

/// R5: a server that is killed takes the real child with it (Linux).
#[cfg(target_os = "linux")]
#[test]
fn a_killed_server_takes_the_real_child_with_it() {
    let pg = crate::support::TestPostgres::start();
    let dir = tempfile::tempdir().unwrap();
    // The server starts the real binary: the configuration names it in place of the fake.
    let fake = crate::support::fake_binary(dir.path())
        .display()
        .to_string();
    let (mut child, _port, _) =
        crate::binary::start_server_with(dir.path(), &pg, Duration::from_secs(60), |path| {
            let text = std::fs::read_to_string(path)
                .unwrap()
                .replace(&fake, &rustfs_bin().display().to_string());
            std::fs::write(path, text).unwrap();
        });
    let child_pid = rustfs_pid_of(child.id());
    child.kill().unwrap();
    let _ = child.wait();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::path::Path::new(&format!("/proc/{child_pid}")).exists() && !is_zombie(child_pid) {
        assert!(
            Instant::now() < deadline,
            "the object server outlived the server"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The ID of the child process of `parent` (Linux): the process whose parent it is.
#[cfg(target_os = "linux")]
fn rustfs_pid_of(parent: u32) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found = std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .find(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                    // The fields after the command's closing parenthesis: state, then parent.
                    let rest = stat.rsplit(')').next().unwrap_or("");
                    rest.split_whitespace()
                        .nth(1)
                        .and_then(|p| p.parse::<u32>().ok())
                        == Some(parent)
                })
            });
        if let Some(pid) = found {
            return pid;
        }
        assert!(Instant::now() < deadline, "the server has no child");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| {
            s.rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z')
        })
        .unwrap_or(true)
}

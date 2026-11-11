//! `ngr-server` as a process: usage and start failures, health over TCP, and the signal stop.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn server(args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ngr-server"));
    c.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_exit(child: &mut Child, limit: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the process did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn get_health(port: u16) -> Option<String> {
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        s,
        "GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut text = String::new();
    s.read_to_string(&mut text).ok()?;
    Some(text)
}

fn write_config(dir: &Path, port: u16) -> std::path::PathBuf {
    let path = dir.join("ngr-server.toml");
    std::fs::write(
        &path,
        format!("[http]\nport = {port}\nshutdown_grace_secs = 2\n"),
    )
    .unwrap();
    path
}

#[test]
fn no_config_argument_is_a_usage_error() {
    let status = wait_exit(&mut server(&[]).spawn().unwrap(), Duration::from_secs(20));
    assert_eq!(status.code(), Some(2));
}

#[test]
fn an_unknown_argument_is_a_usage_error() {
    let status = wait_exit(
        &mut server(&["--config", "x.toml", "--port", "1"])
            .spawn()
            .unwrap(),
        Duration::from_secs(20),
    );
    assert_eq!(status.code(), Some(2));
}

#[test]
fn an_unreadable_config_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.toml");
    let status = wait_exit(
        &mut server(&["--config", missing.to_str().unwrap()])
            .spawn()
            .unwrap(),
        Duration::from_secs(20),
    );
    assert_eq!(status.code(), Some(1));
}

#[test]
fn an_invalid_config_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.toml");
    std::fs::write(&path, "[http]\nport = 0\n").unwrap();
    let status = wait_exit(
        &mut server(&["--config", path.to_str().unwrap()])
            .spawn()
            .unwrap(),
        Duration::from_secs(20),
    );
    assert_eq!(status.code(), Some(1));
}

#[test]
fn a_taken_port_exits_1() {
    let holder = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = holder.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), port);
    let status = wait_exit(
        &mut server(&["--config", path.to_str().unwrap()])
            .spawn()
            .unwrap(),
        Duration::from_secs(20),
    );
    assert_eq!(status.code(), Some(1));
}

/// Starts the server on a port that was free a moment ago. Another process may take the port
/// before the child binds it; the child then exits 1, and the start is tried again with a new
/// port. The reply must be the server's own, so a stranger on the port cannot pass for it.
fn start_server(dir: &Path) -> (Child, u16, String) {
    for _attempt in 0..5 {
        let port = free_port();
        let path = write_config(dir, port);
        let mut child = server(&["--config", path.to_str().unwrap()])
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let reply = get_health(port).filter(|t| t.contains(r#"{"status":"ok"}"#));
            if let (Some(text), None) = (reply, child.try_wait().unwrap()) {
                return (child, port, text);
            }
            if let Some(status) = child.try_wait().unwrap() {
                assert_eq!(status.code(), Some(1), "the server ended unexpectedly");
                break; // the bind lost the race: try another port
            }
            assert!(Instant::now() < deadline, "the server never answered");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    panic!("no free port could be bound in five attempts");
}

#[test]
fn it_serves_health_and_stops_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, _port, reply) = start_server(dir.path());
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");

    #[cfg(unix)]
    {
        let sent = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        let status = wait_exit(&mut child, Duration::from_secs(10));
        assert_eq!(status.code(), Some(0));
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(unix)]
#[test]
fn a_second_signal_during_the_grace_period_stops_it_at_once() {
    let dir = tempfile::tempdir().unwrap();
    // The configured grace period is 2 s; the connection below holds the stop for that long.
    let (mut child, port, _) = start_server(dir.path());
    let mut held = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    held.write_all(b"GET /api/v1/health HTTP/1.1\r\n").unwrap();
    let pid = child.id().to_string();
    // Two different signals: a second SIGTERM sent before the first is taken would be merged
    // with it by the kernel, which delivers one pending signal of a kind, and the stop would
    // then wait out the grace period.
    for signal in ["-TERM", "-INT"] {
        let sent = Command::new("kill").args([signal, &pid]).status().unwrap();
        assert!(sent.success());
    }
    let started = Instant::now();
    let status = wait_exit(&mut child, Duration::from_secs(10));
    assert_eq!(status.code(), Some(0));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

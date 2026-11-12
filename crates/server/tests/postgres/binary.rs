//! `ngr-server` as a process: usage and start failures, health over TCP against a throwaway
//! PostgreSQL, a start that the database refuses, and the signal stop.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::support::{TestPostgres, random_password, refused_url};

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

fn write_config(dir: &Path, port: u16, database_url: &str) -> std::path::PathBuf {
    let path = dir.join("ngr-server.toml");
    std::fs::write(
        &path,
        format!(
            "[http]\nport = {port}\nshutdown_grace_secs = 2\n\
             [database]\nurl = \"{database_url}\"\nconnect_timeout_secs = 2\n"
        ),
    )
    .unwrap();
    path
}

/// Runs `sql` on the cluster from a synchronous test.
fn execute(pg: &TestPostgres, sql: &'static str) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(pg.execute(sql));
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
    let pg = TestPostgres::start();
    let holder = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = holder.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), port, &pg.url());
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
fn start_server(dir: &Path, pg: &TestPostgres) -> (Child, u16, String) {
    for _attempt in 0..5 {
        let port = free_port();
        let path = write_config(dir, port, &pg.url());
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
    let pg = TestPostgres::start();
    let dir = tempfile::tempdir().unwrap();
    let (mut child, _port, reply) = start_server(dir.path(), &pg);
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
    let pg = TestPostgres::start();
    let dir = tempfile::tempdir().unwrap();
    // The configured grace period is 2 s; the connection below holds the stop for that long.
    let (mut child, port, _) = start_server(dir.path(), &pg);
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

#[test]
fn a_refused_database_exits_1_within_the_connect_timeout_and_never_listens() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    // A value made at run time, so the test holds no fixed secret.
    let secret = random_password();
    let path = write_config(dir.path(), port, &refused_url(&secret));
    let mut command = server(&["--config", path.to_str().unwrap()]);
    command.stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    let started = Instant::now();
    let output = wait_output(child, Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(1));
    // The configured timeout is 2 s; the rest is process start.
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    let log = String::from_utf8_lossy(&output.stderr);
    // The server never got as far as listening (the port itself may belong to anyone by now).
    assert!(!log.contains("listening"), "{log}");
    assert!(log.contains("cannot connect to the database"), "{log}");
    assert!(!log.contains(&secret), "the log shows the password: {log}");
}

#[test]
fn a_changed_migration_exits_1_and_never_listens() {
    let pg = TestPostgres::start();
    let dir = tempfile::tempdir().unwrap();
    let (mut child, _port, _) = start_server(dir.path(), &pg);
    let _ = child.kill();
    let _ = child.wait();

    execute(
        &pg,
        "UPDATE _sqlx_migrations SET checksum = decode('00', 'hex') WHERE version = 1",
    );
    let port = free_port();
    let path = write_config(dir.path(), port, &pg.url());
    let mut command = server(&["--config", path.to_str().unwrap()]);
    command.stderr(Stdio::piped());
    let output = wait_output(command.spawn().unwrap(), Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(1));
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(!log.contains("listening"), "{log}");
    assert!(log.contains("modified"), "{log}");
}

#[test]
fn a_url_with_a_parameter_is_refused_without_showing_it() {
    let dir = tempfile::tempdir().unwrap();
    let secret = random_password();
    for parameter in ["sslpassword", "pasword"] {
        let url = format!("postgres://ngr:pw@127.0.0.1:5432/db?{parameter}={secret}");
        let path = write_config(dir.path(), free_port(), &url);
        let mut command = server(&["--config", path.to_str().unwrap()]);
        command.stderr(Stdio::piped());
        let output = wait_output(command.spawn().unwrap(), Duration::from_secs(30));
        assert_eq!(output.status.code(), Some(1));
        let log = String::from_utf8_lossy(&output.stderr);
        assert!(log.contains("database.url is not a"), "{log}");
        assert!(!log.contains(&secret), "the log shows the value: {log}");
    }
}

fn wait_output(child: Child, limit: Duration) -> std::process::Output {
    let id = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(limit) {
        Ok(output) => output.unwrap(),
        Err(_) => panic!("process {id} did not exit within {limit:?}"),
    }
}

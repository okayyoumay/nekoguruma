//! A stand-in for the object server (RustFS) that the server's tests start (ADR-491). It takes
//! the arguments and the environment the server gives RustFS (`server`, `RUSTFS_ADDRESS`,
//! `RUSTFS_VOLUMES`, `RUSTFS_ACCESS_KEY_FILE`, `RUSTFS_SECRET_KEY_FILE`), listens on the address,
//! answers `GET /health/ready`, and misbehaves as the file `fake-object-server.cfg` in its
//! working directory (the server's data directory) says. The file has one `key value` per line:
//!
//! - `mode`: `ok` (the default), `never-ready` (503), `garbage` (a reply that is no HTTP),
//!   `silent` (never listens), `hang` (accepts and never answers) or `exit` (ends at once, with
//!   the code of `code`, default 3);
//! - `ready_after_ms <n>`: 503 until that long after the start;
//! - `die_after_ms <n>` with `die_code <n>` or `die_signal 1`: ends by itself that long after
//!   its first 200 (so after the server's readiness probe succeeded) (`die_signal`: killed by SIGKILL on Unix);
//! - `s3_status <n>`: answers every request outside `/health/` with that status and no body
//!   (the default is 200, which the store's HeadBucket accepts as "the bucket exists"), so that a
//!   test can make the store's open fail (403) or retry (503);
//! - `ignore_term 1`: ignores SIGTERM (Unix); otherwise SIGTERM ends it with code 0;
//! - `print_secret 1`: prints the secret key it reads from its file, with a long line and a
//!   control character, to standard output and standard error;
//! - `grandchild same` or `grandchild group`: starts a `sleep 4` (Unix) that inherits its output
//!   pipes and outlives it, in its group or in one of its own; its PID is written to `grandchild.pid`;
//! - `report 1`: writes `report.txt` (its arguments, its environment, its working directory and,
//!   on Linux, its open descriptors).
//!
//! - `lose_port_once 1`: the first start in a data directory ends with status 98 as a lost port
//!   does (below), after `print_secret`'s output and before it binds; the later starts go on.
//!
//! A bind that fails because the address is in use ends it with status 98. The tests tell a lost
//! port from an end on purpose by that status, so `code` and `die_code` refuse 98.
//!
//! It always writes its process ID to `pid` and prints a line when it is up.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

#[allow(
    unsafe_code,
    reason = "libc FFI with no safe std equivalent, Unix only: `signal` installs a signal handler (ADR-502)"
)]
fn main() {
    let cfg = read_config();
    let get = |key: &str| cfg.get(key).map(String::as_str);
    let number = |key: &str| get(key).and_then(|v| v.parse::<u64>().ok());
    let started = Instant::now();
    let _ = std::fs::write("pid", std::process::id().to_string());

    #[cfg(unix)]
    {
        #[allow(
            unsafe_code,
            reason = "libc FFI with no safe std equivalent, Unix only: `_exit` ends the process without unwinding, `signal` installs a signal handler (ADR-502)"
        )]
        extern "C" fn on_term(_: libc::c_int) {
            // SAFETY: `_exit` is async-signal-safe.
            unsafe { libc::_exit(0) }
        }
        let handler = if get("ignore_term") == Some("1") {
            libc::SIG_IGN
        } else {
            on_term as extern "C" fn(libc::c_int) as *const () as libc::sighandler_t
        };
        // SAFETY: installs a handler that only calls `_exit`, or ignores the signal.
        unsafe { libc::signal(libc::SIGTERM, handler) };
    }

    if get("report") == Some("1") {
        report();
    }
    #[cfg(unix)]
    if get("grandchild").is_some() {
        use std::os::unix::process::CommandExt;
        let mut sleeper = std::process::Command::new("sleep");
        sleeper.arg("4");
        // `grandchild group`: in a group of its own, out of reach of a kill of the child's group.
        if get("grandchild") == Some("group") {
            sleeper.process_group(0);
        }
        if let Ok(sleeper) = sleeper.spawn() {
            let _ = std::fs::write("grandchild.pid", sleeper.id().to_string());
        }
    }
    // Echoes the text of the file the server names, the redaction tests' input (the server
    // must replace it in what it forwards). A test binary only: it holds no credentials of
    // its own.
    if get("print_secret") == Some("1") {
        let path = std::env::var_os("RUSTFS_SECRET_KEY_FILE").unwrap_or_default();
        let text = std::fs::read_to_string(path).unwrap_or_default();
        println!("echo is {text}");
        eprintln!(
            "\u{1b}[31mecho again {text}\u{1b}[0m {}",
            "z".repeat(20_000)
        );
    }
    // 98 is the status of a lost port (below): a test's own end must not look like one.
    for key in ["code", "die_code"] {
        if number(key) == Some(98) {
            eprintln!("{key} 98 is reserved for a lost port");
            std::process::exit(2);
        }
    }
    if get("lose_port_once") == Some("1")
        && std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open("lost-port-once")
            .is_ok()
    {
        eprintln!("cannot bind: the port is lost (test hook)");
        std::process::exit(98);
    }
    match get("mode").unwrap_or("ok") {
        "exit" => std::process::exit(number("code").unwrap_or(3) as i32),
        "silent" => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
        _ => {}
    }

    let address = std::env::var("RUSTFS_ADDRESS").expect("RUSTFS_ADDRESS");
    let listener = match TcpListener::bind(&address) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            // The tests pick a port that was free a moment before, and tell this end (a lost
            // port, worth another try) from any other by the status; it matches
            // `LOST_PORT_EXIT` in the tests' `support.rs`.
            eprintln!("cannot bind {address}: {error}");
            std::process::exit(98);
        }
        Err(error) => panic!("bind the address: {error}"),
    };
    println!("fake object server up on {address}");
    let mode = get("mode").unwrap_or("ok").to_owned();
    let ready_after = Duration::from_millis(number("ready_after_ms").unwrap_or(0));
    let s3_status = number("s3_status");
    // `die_after_ms` counts from the first 200 the fake gives, so that a test's delay holds
    // however long the server took to reach it.
    let die = number("die_after_ms").map(|ms| {
        (
            ms,
            get("die_signal") == Some("1"),
            number("die_code").unwrap_or(4) as i32,
        )
    });
    for stream in listener.incoming().flatten() {
        let mode = mode.clone();
        std::thread::spawn(move || {
            answer(
                stream,
                &mode,
                started.elapsed() >= ready_after,
                die,
                s3_status,
            )
        });
    }
}

fn read_config() -> HashMap<String, String> {
    let text = std::fs::read_to_string("fake-object-server.cfg").unwrap_or_default();
    text.lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

#[allow(
    unsafe_code,
    reason = "libc FFI with no safe std equivalent, Unix only: `raise` raises a signal (ADR-502)"
)]
fn answer(
    mut stream: TcpStream,
    mode: &str,
    ready: bool,
    die: Option<(u64, bool, i32)>,
    s3_status: Option<u64>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut buf = [0u8; 512];
    while head.len() < 4096 && !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    // The request target: the second word of the request line.
    let target = head
        .split(|&b| b == b' ')
        .nth(1)
        .map(|t| t.to_vec())
        .unwrap_or_default();
    if let Some(status) = s3_status.filter(|_| !target.starts_with(b"/health/")) {
        let _ = stream.write_all(
            format!("HTTP/1.1 {status} Steered\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .as_bytes(),
        );
        return;
    }
    let reply: &[u8] = match mode {
        "hang" => {
            std::thread::sleep(Duration::from_secs(60));
            return;
        }
        "garbage" => b"nonsense\r\n\r\n",
        "never-ready" => {
            b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        }
        _ if ready => b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        _ => b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    };
    let _ = stream.write_all(reply);
    if ready && mode != "garbage" && mode != "never-ready" {
        static ONCE: std::sync::Once = std::sync::Once::new();
        if let Some((ms, signal, code)) = die {
            ONCE.call_once(|| {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(ms));
                    #[cfg(unix)]
                    if signal {
                        // SAFETY: raises SIGKILL on this process.
                        unsafe { libc::raise(libc::SIGKILL) };
                    }
                    let _ = signal;
                    std::process::exit(code);
                });
            });
        }
    }
}

#[allow(
    unsafe_code,
    reason = "libc FFI with no safe std equivalent, Unix only: `getpgrp` reads the process group id, `getppid` reads the parent process id (ADR-502)"
)]
fn report() {
    let mut text = String::new();
    for arg in std::env::args().skip(1) {
        text.push_str(&format!("arg {arg}\n"));
    }
    for (key, value) in std::env::vars() {
        text.push_str(&format!("env {key}={value}\n"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        text.push_str(&format!("cwd {}\n", cwd.display()));
    }
    if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
        for entry in entries.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path()) {
                text.push_str(&format!(
                    "fd {} {}\n",
                    entry.file_name().to_string_lossy(),
                    target.display()
                ));
            }
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: plain queries of this process's group and parent.
        let (pgid, ppid) = unsafe { (libc::getpgrp(), libc::getppid()) };
        text.push_str(&format!(
            "pgid {pgid}\nppid {ppid}\npid {}\n",
            std::process::id()
        ));
    }
    let _ = std::fs::write("report.txt", text);
}

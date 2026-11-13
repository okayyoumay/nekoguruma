//! `ngr-agent connect` as a process (design 5.3, 6.4, ADR-471): the command's order, its exit
//! codes and its stop. The command against a real server is driven by the server's
//! `tests/postgres/agent_connect.rs`. Every process gets its own `HOME`, `XDG_STATE_HOME`,
//! `LOCALAPPDATA` and state directory.
//!
//! A release build has no plain-file store and ignores the state directory override, so the tests
//! that need a key or a planted record run in debug builds only; the command-line and
//! not-registered cases run in both.

use std::io::Read;
#[cfg(debug_assertions)]
use std::io::{BufRead, BufReader};
#[cfg(debug_assertions)]
use std::net::{Ipv4Addr, TcpListener};
#[cfg(debug_assertions)]
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
#[cfg(debug_assertions)]
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

#[cfg(debug_assertions)]
use agent::registration::record::{RECORD_FILE, Record};
#[cfg(debug_assertions)]
use shared_crypto::{Signer, SigningKey};

const KEY_STORE_VAR: &str = "NGR_AGENT_KEY_STORE";
const STATE_DIR_VAR: &str = "NGR_AGENT_STATE_DIR";
#[cfg(debug_assertions)]
const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const LIMIT: Duration = Duration::from_secs(60);

struct Env {
    home: tempfile::TempDir,
    state: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        Env {
            home: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
        }
    }

    /// `ngr-agent` with the account's state redirected into the temporary directories. The
    /// plain-file store is selected in debug builds.
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ngr-agent"));
        command
            .env("HOME", self.home.path())
            .env("XDG_STATE_HOME", self.home.path().join("xdg"))
            .env("LOCALAPPDATA", self.home.path().join("local"))
            .env_remove(KEY_STORE_VAR)
            .env_remove(STATE_DIR_VAR)
            .stdin(Stdio::null());
        if cfg!(debug_assertions) {
            command
                .env(KEY_STORE_VAR, "file")
                .env(STATE_DIR_VAR, self.state.path());
        }
        command
    }

    fn connect(&self) -> Command {
        let mut command = self.command();
        command.args(["connect", "--mode", "user"]);
        command
    }

    fn identity_dir(&self) -> PathBuf {
        self.state.path().join("identity")
    }

    #[cfg(debug_assertions)]
    /// Writes a record for `fingerprint` naming a loopback origin on `port`.
    fn register(&self, fingerprint: &str, port: u16) {
        let instruction = SigningKey::generate().unwrap().public_key();
        let record = Record {
            agent_id: AGENT_ID.to_owned(),
            owner_id: "7b1d0c2e-4a5f-4c6d-9e8f-0a1b2c3d4e5f".to_owned(),
            preset: "personal".to_owned(),
            allow_remote: false,
            server_origin: format!("http://127.0.0.1:{port}"),
            instruction_key_pem: instruction.to_spki_pem(),
            instruction_key_id: instruction.fingerprint_hex(),
            agent_key_fingerprint: fingerprint.to_owned(),
        };
        let dir = self.state.path().join("registration");
        make_private_dir(self.state.path());
        make_private_dir(&dir);
        std::fs::write(dir.join(RECORD_FILE), serde_json::to_vec(&record).unwrap()).unwrap();
    }
}

#[cfg(debug_assertions)]
fn make_private_dir(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A loopback port nothing listens on.
#[cfg(debug_assertions)]
fn closed_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.local_addr().unwrap().port()
}

/// Runs `command` to its end within [`LIMIT`].
fn run(mut command: Command) -> Output {
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    finish(child)
}

fn finish(mut child: Child) -> Output {
    let deadline = Instant::now() + LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the process did not exit within {LIMIT:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_end(&mut stdout).unwrap();
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_end(&mut stderr).unwrap();
    }
    Output {
        status,
        stdout,
        stderr,
    }
}

fn assert_exit(output: &Output, code: i32, contains: &str) {
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(code), "{stderr}");
    assert!(output.stdout.is_empty(), "stdout: {}", text(&output.stdout));
    assert!(
        stderr.contains(&format!("ngr-agent: connect: {contains}")),
        "{stderr}"
    );
}

#[test]
fn a_bad_command_line_is_exit_2() {
    let env = Env::new();
    for args in [
        &["connect"][..],
        &["connect", "--mode"],
        &["connect", "--mode", "machine"],
        &["connect", "--mode", "user", "--mode", "user"],
        &[
            "connect",
            "--mode",
            "user",
            "--server",
            "http://127.0.0.1:1",
        ],
        &["connect", "--mode", "user", "extra"],
    ] {
        let output = run({
            let mut command = env.command();
            command.args(args);
            command
        });
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(output.stdout.is_empty());
        assert!(stderr.contains("usage: ngr-agent"), "{stderr}");
        assert!(stderr.contains("ngr-agent connect"), "{stderr}");
        assert!(
            !stderr.lines().next().unwrap().contains("127.0.0.1"),
            "a value is echoed: {stderr}"
        );
    }
    assert!(!env.state.path().join("control").exists());
    assert!(!env.identity_dir().exists());
}

/// Without a record the command ends with "not registered", before the key store is touched.
#[test]
fn an_unregistered_agent_is_exit_1_and_the_key_is_not_created() {
    let env = Env::new();
    let output = run(env.connect());
    assert_exit(&output, 1, "not registered");
    assert!(!env.identity_dir().exists(), "the key store was touched");
}

/// A record that does not read says to register again. (A release build ignores the state
/// directory override, so a record can be planted in a debug build only.)
#[cfg(debug_assertions)]
#[test]
fn a_record_that_does_not_read_is_exit_1_register_again() {
    let env = Env::new();
    env.register(&"ab".repeat(32), closed_port());
    let path = env.state.path().join("registration").join(RECORD_FILE);
    for content in [&b"{}"[..], b"not json"] {
        std::fs::write(&path, content).unwrap();
        let output = run(env.connect());
        assert_exit(&output, 1, "the registration record cannot be used");
        assert!(text(&output.stderr).contains("register again"));
        assert!(!env.identity_dir().exists());
    }
    // An origin that is not loopback http is refused by the same reader (https included).
    env.register(&"ab".repeat(32), 8443);
    let text_now = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text_now.replace("http://127.0.0.1:8443", "https://127.0.0.1:8443"),
    )
    .unwrap();
    assert_exit(
        &run(env.connect()),
        1,
        "the registration record cannot be used",
    );
}

/// A debug build without the store-selecting variable has no key store, like a release build:
/// a registered agent exits 1 without a connection.
#[cfg(debug_assertions)]
#[test]
fn without_a_store_the_exit_is_1_and_nothing_connects() {
    let env = Env::new();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    env.register(&"ab".repeat(32), listener.local_addr().unwrap().port());
    let mut command = env.connect();
    command.env_remove(KEY_STORE_VAR);
    let output = run(command);
    assert_exit(&output, 1, "no key store");
    assert!(listener.accept().is_err(), "the agent connected");
    assert!(!env.identity_dir().exists());
}

#[cfg(debug_assertions)]
mod with_store {
    use super::*;

    /// The key's fingerprint, creating the key with `ngr-agent identity` when needed.
    fn fingerprint(env: &Env) -> String {
        let output = run({
            let mut command = env.command();
            command.args(["identity", "--mode", "user"]);
            command
        });
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        json["fingerprint"].as_str().unwrap().to_owned()
    }

    fn key_files(env: &Env) -> Vec<(PathBuf, Vec<u8>)> {
        std::fs::read_dir(env.identity_dir())
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| path.extension().is_some_and(|e| e == "pk8"))
                    .map(|path| {
                        let bytes = std::fs::read(&path).unwrap();
                        (path, bytes)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A key the command had to create is not the registered one: register again, and the key it
    /// made stays (a later registration issues a code for it).
    #[test]
    fn a_key_created_by_the_command_is_exit_1_register_again() {
        let env = Env::new();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        env.register(&"ab".repeat(32), listener.local_addr().unwrap().port());
        let output = run(env.connect());
        assert_exit(&output, 1, "register again");
        assert_eq!(key_files(&env).len(), 1);
        assert!(listener.accept().is_err(), "the agent connected");
    }

    /// A key that is not the record's: register again, the key untouched, no connection.
    #[test]
    fn a_replaced_key_is_exit_1_register_again() {
        let env = Env::new();
        let fingerprint = fingerprint(&env);
        let before = key_files(&env);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let other = SigningKey::generate()
            .unwrap()
            .public_key()
            .fingerprint_hex();
        assert_ne!(other, fingerprint);
        env.register(&other, listener.local_addr().unwrap().port());
        let output = run(env.connect());
        assert_exit(&output, 1, "register again");
        assert_eq!(key_files(&env), before);
        assert!(listener.accept().is_err(), "the agent connected");
    }

    /// A key that is present and cannot be used is exit 4 and is not replaced.
    #[test]
    fn an_unusable_key_is_exit_4_and_is_not_replaced() {
        let env = Env::new();
        let fingerprint = fingerprint(&env);
        env.register(&fingerprint, closed_port());
        let (path, _) = key_files(&env).remove(0);
        std::fs::write(&path, b"not a key").unwrap();
        let output = run(env.connect());
        assert_exit(&output, 4, "the stored agent key cannot be used");
        assert_eq!(std::fs::read(&path).unwrap(), b"not a key");
    }

    /// A running `connect` and the lines of its operational log.
    struct Running {
        child: Option<Child>,
        lines: Receiver<String>,
    }

    fn start(env: &Env) -> Running {
        let mut child = env
            .connect()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Running {
            child: Some(child),
            lines,
        }
    }

    impl Running {
        fn wait_for(&self, word: &str) {
            let deadline = Instant::now() + LIMIT;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.lines.recv_timeout(left) {
                    Ok(line) if line.contains(word) => return,
                    Ok(_) => {}
                    Err(error) => panic!("no log line with {word:?}: {error}"),
                }
            }
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[cfg(unix)]
    fn signal(child: &Child, signal: i32) {
        // SAFETY: `kill` takes a process ID and a signal number and touches no memory.
        let result = unsafe { libc::kill(child.id() as i32, signal) };
        assert_eq!(result, 0);
    }

    /// A running session holds the connection lock, so a second command is exit 3 (on every
    /// platform; the first is killed when the test ends).
    #[test]
    fn a_running_session_holds_its_lock() {
        let env = Env::new();
        let fingerprint = fingerprint(&env);
        // Nothing listens: the session retries until it is stopped.
        env.register(&fingerprint, closed_port());
        let first = start(&env);
        first.wait_for("session started");
        let second = run(env.connect());
        assert_exit(&second, 3, "a control channel session is already running");
    }

    /// SIGINT or SIGTERM ends a running session with 0 and frees the lock.
    #[cfg(unix)]
    #[test]
    fn a_signal_stops_a_running_session_with_exit_0() {
        for stop in [libc::SIGINT, libc::SIGTERM] {
            let env = Env::new();
            let fingerprint = fingerprint(&env);
            env.register(&fingerprint, closed_port());
            let mut first = start(&env);
            first.wait_for("session started");
            signal(first.child.as_ref().unwrap(), stop);
            let output = finish(first.child.take().unwrap());
            assert_eq!(output.status.code(), Some(0), "signal {stop}");
            assert!(output.stdout.is_empty());
            // The lock is released: the next one starts again.
            let again = start(&env);
            again.wait_for("session started");
            signal(again.child.as_ref().unwrap(), libc::SIGTERM);
        }
    }
}

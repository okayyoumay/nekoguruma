//! `ngr-agent register` as a process (design 6.4, ADR-463, ADR-470): the binary against a stub
//! server the test runs, with the case table's rows named in each test's comment. Every process
//! gets its own `HOME`, `XDG_STATE_HOME`, `LOCALAPPDATA` and state directory. Codes and keys are
//! made at run time.
//!
//! A release build has no plain-file store, so the tests that need a key run in debug builds
//! only; the command-line cases and the no-store case run in both.

use std::io::Read;
#[cfg(debug_assertions)]
use std::io::Write;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
#[cfg(debug_assertions)]
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(debug_assertions)]
use agent::registration::record::{RECORD_FILE, Record, TEMP_FILE};
#[cfg(debug_assertions)]
use base64::Engine;
#[cfg(debug_assertions)]
use base64::engine::general_purpose::STANDARD;
#[cfg(debug_assertions)]
use shared_crypto::registration::{Preset, RegistrationCode, ResponseTranscript, sign_response};
#[cfg(debug_assertions)]
use shared_crypto::{PublicKey, Signer, SigningKey};

const KEY_STORE_VAR: &str = "NGR_AGENT_KEY_STORE";
const STATE_DIR_VAR: &str = "NGR_AGENT_STATE_DIR";
#[cfg(debug_assertions)]
const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
#[cfg(debug_assertions)]
const OWNER_ID: &str = "7b1d0c2e-4a5f-4c6d-9e8f-0a1b2c3d4e5f";
const LIMIT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------------------------
// The environment

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
            .env_remove(STATE_DIR_VAR);
        if cfg!(debug_assertions) {
            command
                .env(KEY_STORE_VAR, "file")
                .env(STATE_DIR_VAR, self.state.path());
        }
        command
    }

    #[cfg(debug_assertions)]
    fn registration_dir(&self) -> PathBuf {
        self.state.path().join("registration")
    }

    #[cfg(debug_assertions)]
    fn record_path(&self) -> PathBuf {
        self.registration_dir().join(RECORD_FILE)
    }

    #[cfg(debug_assertions)]
    /// The agent key's fingerprint, creating the key with `ngr-agent identity` when needed.
    fn fingerprint(&self) -> String {
        let output = self
            .command()
            .args(["identity", "--mode", "user"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        json["fingerprint"].as_str().unwrap().to_owned()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

/// Waits for `child` within [`LIMIT`] and returns what it printed.
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
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
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

#[cfg(debug_assertions)]
/// Starts `command` with `input` on its standard input, which is then closed.
fn spawn_with(mut command: Command, input: &[u8]) -> Child {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(input);
    drop(stdin);
    child
}

#[cfg(debug_assertions)]
fn run_with(command: Command, input: &[u8]) -> Output {
    finish(spawn_with(command, input))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `register` arguments for a stub on `port`, with the server key `key_id`.
fn register_args(port: u16, key_id: &str) -> Vec<String> {
    [
        "register",
        "--mode",
        "user",
        "--server",
        &format!("http://127.0.0.1:{port}"),
        "--server-key",
        key_id,
    ]
    .map(str::to_owned)
    .to_vec()
}

// ---------------------------------------------------------------------------------------------
// The stub server

/// What the stub read of the one request it served.
#[cfg_attr(
    not(debug_assertions),
    expect(dead_code, reason = "only the tests that need a key read the request")
)]
struct Seen {
    head: String,
    body: Vec<u8>,
}

#[cfg(debug_assertions)]
impl Seen {
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

/// Serves at most one connection: reads the request, calls `respond` with it and the stream, and
/// returns what it read. [`Stub::finish`] says whether a connection came at all.
struct Stub {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Option<Seen>>,
}

impl Stub {
    fn start(respond: impl FnOnce(&Seen, &mut TcpStream) + Send + 'static) -> Stub {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(_) if flag.load(Ordering::SeqCst) => return None,
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(20))).ok();
            let mut raw = Vec::new();
            let mut byte = [0u8; 1];
            while !raw.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    return None;
                }
                raw.push(byte[0]);
            }
            let head = text(&raw);
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            stream.read_exact(&mut body).unwrap();
            let seen = Seen { head, body };
            respond(&seen, &mut stream);
            Some(seen)
        });
        Stub { port, stop, thread }
    }

    #[cfg(debug_assertions)]
    /// Answers every request with `bytes`.
    fn raw(bytes: Vec<u8>) -> Stub {
        Stub::start(move |_, stream| {
            let _ = stream.write_all(&bytes);
        })
    }

    /// Ends the stub: what it read, or `None` when nothing connected.
    fn finish(self) -> Option<Seen> {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.join().unwrap()
    }
}

/// A listener nothing connects to, for the no-connection checks.
fn silent() -> Stub {
    Stub::start(|_, _| panic!("nothing should connect"))
}

#[cfg(debug_assertions)]
fn http(status: u16, headers: &[&str], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} X\r\n").into_bytes();
    for header in headers {
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(
        format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(body);
    out
}

#[cfg(debug_assertions)]
/// The answer an honest server gives to `seen`, as a JSON value, signed by `server`.
fn answer(seen: &Seen, server: &SigningKey) -> serde_json::Value {
    let request = seen.json();
    let code = RegistrationCode::parse(request["code"].as_str().unwrap()).unwrap();
    let agent = PublicKey::from_spki_pem(request["publicKey"].as_str().unwrap()).unwrap();
    let transcript =
        ResponseTranscript::new(&code, &agent, AGENT_ID, OWNER_ID, Preset::Personal, false)
            .unwrap();
    let signature = sign_response(server, &transcript).unwrap();
    let key = server.public_key();
    serde_json::json!({
        "agentId": AGENT_ID,
        "ownerId": OWNER_ID,
        "preset": "personal",
        "allowRemote": false,
        "instructionKey": key.to_spki_pem(),
        "instructionKeyId": key.fingerprint_hex(),
        "signature": STANDARD.encode(signature.to_bytes()),
    })
}

#[cfg(debug_assertions)]
fn created(value: &serde_json::Value) -> Vec<u8> {
    http(
        201,
        &["Content-Type: application/json"],
        value.to_string().as_bytes(),
    )
}

#[cfg(debug_assertions)]
/// A stub that answers 201 with what `change` makes of the honest answer.
fn honest(
    server: &Arc<SigningKey>,
    change: impl FnOnce(&mut serde_json::Value) + Send + 'static,
) -> Stub {
    let server = server.clone();
    Stub::start(move |seen, stream| {
        let mut value = answer(seen, &server);
        change(&mut value);
        let _ = stream.write_all(&created(&value));
    })
}

#[cfg(debug_assertions)]
fn new_server() -> Arc<SigningKey> {
    Arc::new(SigningKey::generate().unwrap())
}

#[cfg(debug_assertions)]
fn key_id(server: &SigningKey) -> String {
    server.public_key().fingerprint_hex()
}

#[cfg(debug_assertions)]
/// Every file under `dirs`, with its bytes.
fn files_under(dirs: &[&Path]) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut pending: Vec<PathBuf> = dirs.iter().map(|d| d.to_path_buf()).collect();
    while let Some(path) = pending.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                pending.extend(entries.flatten().map(|e| e.path()));
            }
        } else if meta.is_file() {
            found.push((path.clone(), std::fs::read(&path).unwrap_or_default()));
        }
    }
    found
}

#[cfg(debug_assertions)]
/// P2: the code, in any case, is in no output and in no file the command left.
fn assert_code_nowhere(env: &Env, code: &RegistrationCode, outputs: &[&Output]) {
    let canonical = code.as_str().to_owned();
    let needles = [canonical.clone(), canonical.to_lowercase()];
    let digits = canonical[5..].to_owned();
    let mut haystacks: Vec<(String, Vec<u8>)> = Vec::new();
    for output in outputs {
        haystacks.push(("stdout".into(), output.stdout.clone()));
        haystacks.push(("stderr".into(), output.stderr.clone()));
    }
    for (path, bytes) in files_under(&[env.state.path(), env.home.path()]) {
        haystacks.push((path.display().to_string(), bytes));
    }
    for (what, bytes) in haystacks {
        let hay = text(&bytes);
        for needle in needles.iter().chain([&digits, &digits.to_lowercase()]) {
            assert!(!hay.contains(needle.as_str()), "the code is in {what}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A. The command line, in both builds

fn assert_usage_error(output: &Output) {
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(output.stdout.is_empty());
    assert!(stderr.starts_with("ngr-agent: "), "{stderr}");
    assert!(stderr.contains("usage: ngr-agent"), "{stderr}");
}

const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A1 to A7, S and K: each is exit 2 before any I/O, and the message never repeats a value.
#[test]
fn a_bad_command_line_is_exit_2_and_nothing_happens() {
    let env = Env::new();
    let marker = format!(
        "marker{}",
        hex(&shared_crypto::random_32().unwrap())[..12].to_owned()
    );
    let ok = |port: &str| -> Vec<String> {
        [
            "register",
            "--mode",
            "user",
            "--server",
            port,
            "--server-key",
            KEY,
        ]
        .map(str::to_owned)
        .to_vec()
    };
    let server = "http://127.0.0.1:9";
    let mut cases: Vec<Vec<String>> = vec![
        // A2: each required flag missing.
        vec!["register".into()],
        ok(server)[..5].to_vec(),
        [&ok(server)[..3], &ok(server)[5..]].concat(),
        ok(server)[..ok(server).len() - 2].to_vec(),
        // A3: a flag twice.
        [ok(server), vec!["--mode".into(), "user".into()]].concat(),
        [ok(server), vec!["--server".into(), server.into()]].concat(),
        [ok(server), vec!["--server-key".into(), KEY.into()]].concat(),
        [ok(server), vec!["--replace".into(), "--replace".into()]].concat(),
        [
            ok(server),
            vec![
                "--code-file".into(),
                "a".into(),
                "--code-file".into(),
                "b".into(),
            ],
        ]
        .concat(),
        // A4: no value, and `--replace true`.
        [ok(server), vec!["--code-file".into()]].concat(),
        vec!["register".into(), "--mode".into()],
        [ok(server), vec!["--replace".into(), "true".into()]].concat(),
        // A5: unknown flags, positionals, `--flag=value`, `--code <x>`.
        [ok(server), vec!["--verbose".into()]].concat(),
        [ok(server), vec![marker.clone()]].concat(),
        [ok(server), vec![format!("--code={marker}")]].concat(),
        [ok(server), vec!["--code".into(), marker.clone()]].concat(),
        [ok(server), vec![format!("--mode={marker}")]].concat(),
        [ok(server), vec![format!("-{marker}")]].concat(),
        // A6: the mode.
        vec![
            "register",
            "--mode",
            "machine",
            "--server",
            server,
            "--server-key",
            KEY,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        vec![
            "register",
            "--mode",
            "User",
            "--server",
            server,
            "--server-key",
            KEY,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        vec![
            "register",
            "--mode",
            "",
            "--server",
            server,
            "--server-key",
            KEY,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        // K2: the server key.
        ok(server)[..6]
            .iter()
            .cloned()
            .chain(["--server-key".into(), marker.clone()])
            .collect(),
        ok(server)[..6]
            .iter()
            .cloned()
            .chain(["--server-key".into(), KEY[..63].into()])
            .collect(),
    ];
    // S4 to S9.
    for bad in [
        "http://localhost:8080",
        "http://LocalHost:8080",
        "http://app.localhost:8080",
        "https://127.0.0.1:8443",
        "https://localhost",
        "http://127.0.0.1:8080/path",
        "http://127.0.0.1:8080//",
        "http://127.0.0.1:8080?q=1",
        "http://127.0.0.1:8080#f",
        "http://user@127.0.0.1:8080",
        "http://[::1]:8080",
        "http://127.0.0.2:8080",
        "http://192.168.1.10:8080",
        "http://example.com:8080",
        "http://127.0.0.1.example.com:8080",
        "http://127.1",
        "http://0x7f000001",
        "http://127.000.0.1",
        "http://127.0.0.1.",
        "http://127.0.0.1:0",
        "http://127.0.0.1:080",
        "http://127.0.0.1:65536",
        "http://127.0.0.1:",
        "127.0.0.1:8080",
        "http:// 127.0.0.1:8080",
        "http://127.0.0.1:8080\u{e9}",
        "",
    ] {
        cases.push(ok(bad));
    }
    for args in &cases {
        let output = env
            .command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_usage_error(&output);
        let stderr = text(&output.stderr);
        let message = stderr.split("usage: ngr-agent").next().unwrap();
        assert!(!message.contains(&marker), "{args:?}: {message}");
        assert!(!message.contains(KEY), "{args:?}: {message}");
        assert!(!message.contains("secret"), "{args:?}: {message}");
    }
    // Nothing was created.
    assert!(is_empty_dir(env.state.path()));
}

/// S5: the https message says why.
#[test]
fn https_is_refused_with_its_reason() {
    let env = Env::new();
    let output = env
        .command()
        .args(register_args(1, KEY))
        .args(["--server", "https://127.0.0.1:1"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_usage_error(&output);
    let output = env
        .command()
        .args([
            "register",
            "--mode",
            "user",
            "--server",
            "https://127.0.0.1:8443",
            "--server-key",
            KEY,
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(text(&output.stderr).contains("https is not supported yet"));
}

/// A7: a value that is not UTF-8.
#[cfg(unix)]
#[test]
fn a_value_that_is_not_utf8_is_exit_2() {
    use std::os::unix::ffi::OsStrExt;
    let env = Env::new();
    let bad = std::ffi::OsStr::from_bytes(&[b'h', 0xff, 0xfe]);
    for position in 0..2 {
        let mut command = env.command();
        command.arg("register");
        if position == 0 {
            command.arg(bad);
        } else {
            command.args(["--mode"]).arg(bad);
        }
        let output = command.stdin(Stdio::null()).output().unwrap();
        assert_usage_error(&output);
        assert!(text(&output.stderr).contains("not valid UTF-8"));
    }
    assert!(is_empty_dir(env.state.path()));
}

/// A8: a terminal on standard input with no `--code-file` is exit 2, and nothing is touched.
#[cfg(target_os = "linux")]
#[test]
fn a_terminal_on_stdin_is_exit_2() {
    use std::os::fd::{FromRawFd, OwnedFd};
    let (mut master, mut slave) = (0, 0);
    // SAFETY: the call fills both descriptors, which are owned below.
    let made = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(made, 0, "a pseudo-terminal");
    // SAFETY: both descriptors are valid and owned by nothing else.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let env = Env::new();
    let stub = silent();
    let output = env
        .command()
        .args(register_args(stub.port, KEY))
        .stdin(Stdio::from(slave))
        .output()
        .unwrap();
    assert_usage_error(&output);
    let stderr = text(&output.stderr);
    assert!(stderr.contains("--code-file"), "{stderr}");
    assert!(stub.finish().is_none());
    assert!(is_empty_dir(env.state.path()));
    drop(master);
}

// ---------------------------------------------------------------------------------------------
// I3, in both builds

/// I3: with no store, exit 1, no connection and an untouched state directory; the code is not
/// even read.
#[test]
fn without_a_store_the_exit_is_1_with_no_connection_and_an_empty_state() {
    let env = Env::new();
    let stub = silent();
    let mut command = env.command();
    command.env_remove(KEY_STORE_VAR).env_remove(STATE_DIR_VAR);
    command.args(register_args(stub.port, KEY));
    // Standard input stays open and unread: the command must not wait for it.
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let held = child.stdin.take().unwrap();
    let output = finish(child);
    drop(held);
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("register: no key store"), "{stderr}");
    assert!(stub.finish().is_none());
    assert!(is_empty_dir(env.state.path()));
}

// ---------------------------------------------------------------------------------------------
// The rest needs the plain-file store

#[cfg(debug_assertions)]
mod with_store {
    use super::*;

    fn code() -> RegistrationCode {
        RegistrationCode::generate().unwrap()
    }

    /// Runs `register` against `stub` with `code` on standard input.
    fn register(env: &Env, stub: &Stub, key_id: &str, code: &str, extra: &[&str]) -> Output {
        let mut command = env.command();
        command.args(register_args(stub.port, key_id)).args(extra);
        run_with(command, code.as_bytes())
    }

    fn assert_exit(output: &Output, code: i32, contains: &str) {
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(code), "{stderr}");
        assert!(output.stdout.is_empty(), "stdout: {}", text(&output.stdout));
        assert!(
            stderr.contains(&format!("ngr-agent: register: {contains}")),
            "{stderr}"
        );
    }

    fn assert_nothing_stored(env: &Env) {
        assert!(!env.record_path().exists());
        assert!(!env.registration_dir().join(TEMP_FILE).exists());
    }

    /// W1, P1, C1, L1, L4, P2: a registration is stored and printed, whatever the typing of the
    /// code, and the code is nowhere.
    #[test]
    fn a_registration_is_stored_printed_and_never_leaks_the_code() {
        let env = Env::new();
        let fingerprint = env.fingerprint();
        let server = new_server();
        let code = code();
        let stub = honest(&server, |_| {});
        // Lower case, outer spaces and a CRLF: the canonical text is what is sent.
        let typed = format!("  {}\r\n", code.as_str().to_lowercase());
        let output = register(&env, &stub, &key_id(&server), &typed, &[]);
        let seen = stub.finish().expect("a request");
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));

        // The request.
        assert!(
            seen.head
                .starts_with("POST /api/v1/agents/register HTTP/1.1\r\n")
        );
        let lower = seen.head.to_ascii_lowercase();
        assert!(lower.contains("host: 127.0.0.1:"));
        assert!(lower.contains("connection: close\r\n"));
        assert!(!lower.contains("origin:"));
        let request = seen.json();
        assert_eq!(request["code"], code.as_str());
        assert_eq!(request["mode"], "user");

        // The output: one line, the eight fields, no PEM.
        let stdout = text(&output.stdout);
        assert_eq!(stdout.lines().count(), 1, "{stdout}");
        let printed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let mut names: Vec<&str> = printed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "agent_id",
                "agent_key_fingerprint",
                "allow_remote",
                "instruction_key_id",
                "owner_id",
                "preset",
                "replaced",
                "server_origin"
            ]
        );
        assert_eq!(printed["agent_id"], AGENT_ID);
        assert_eq!(printed["owner_id"], OWNER_ID);
        assert_eq!(printed["preset"], "personal");
        assert_eq!(printed["allow_remote"], false);
        assert_eq!(printed["replaced"], false);
        assert_eq!(printed["agent_key_fingerprint"], fingerprint.as_str());
        assert_eq!(printed["instruction_key_id"], key_id(&server).as_str());
        assert_eq!(
            printed["server_origin"],
            format!("http://127.0.0.1:{}", stub_port(&seen)).as_str()
        );
        assert!(!stdout.contains("BEGIN"));

        // The record: the reader takes it, and it is private.
        let record = Record::read(&env.record_path()).unwrap();
        assert_eq!(record.agent_id, AGENT_ID);
        assert_eq!(record.agent_key_fingerprint, fingerprint);
        assert_eq!(
            record.instruction_key_pem,
            server.public_key().to_spki_pem()
        );
        assert!(env.registration_dir().join("registration.lock").is_file());
        assert!(!env.registration_dir().join(TEMP_FILE).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(env.record_path()), 0o600);
            assert_eq!(mode(env.registration_dir()), 0o700);
        }
        assert_code_nowhere(&env, &code, &[&output]);
    }

    /// The port of the request's `Host` header.
    fn stub_port(seen: &Seen) -> u16 {
        seen.head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("host: 127.0.0.1:")
                    .map(str::to_owned)
            })
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// I2: a key that had to be created stops the command: exit 1 with the fingerprint, nothing
    /// sent, standard input unread, nothing under `registration`.
    #[test]
    fn a_key_that_was_just_created_stops_the_command_with_its_fingerprint() {
        let env = Env::new();
        let stub = silent();
        let mut command = env.command();
        command.args(register_args(stub.port, KEY));
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Held open and never written: the command must not read it.
        let held = child.stdin.take().unwrap();
        let output = finish(child);
        drop(held);
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(stub.finish().is_none());
        assert!(!env.registration_dir().exists());
        // The key is there now, and the message names its fingerprint.
        let fingerprint = env.fingerprint();
        assert!(
            stderr.contains(&format!(
                "the agent key was just created; issue a code for fingerprint {fingerprint}"
            )),
            "{stderr}"
        );
    }

    /// I6: a key that is present and unusable is exit 4.
    #[test]
    fn an_unusable_key_is_exit_4() {
        let env = Env::new();
        let dir = env.state.path().join("identity");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("nekoguruma-agent-key.user.v1.pk8"), b"not a key").unwrap();
        let stub = silent();
        let output = register(&env, &stub, KEY, "unread", &[]);
        assert_eq!(output.status.code(), Some(4), "{}", text(&output.stderr));
        assert!(stub.finish().is_none());
        assert!(!env.registration_dir().exists());
    }

    /// I5: an identity directory that is unusable is exit 3.
    #[test]
    fn an_unusable_identity_directory_is_exit_3() {
        let env = Env::new();
        std::fs::write(env.state.path().join("identity"), b"a file").unwrap();
        let stub = silent();
        let output = register(&env, &stub, KEY, "unread", &[]);
        assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
        assert!(stub.finish().is_none());
    }

    /// C2: `--code-file` wins and standard input is never read.
    #[test]
    fn a_code_file_wins_over_standard_input() {
        let env = Env::new();
        env.fingerprint();
        let server = new_server();
        let code = code();
        // The file is the operator's, outside the directories the leak check searches.
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("code.txt");
        std::fs::write(&file, format!("{}\n", code.as_str())).unwrap();
        let stub = honest(&server, |_| {});
        let mut command = env.command();
        command
            .args(register_args(stub.port, &key_id(&server)))
            .arg("--code-file")
            .arg(&file);
        // Standard input is a terminal-less garbage pipe, held open and unread.
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut held = child.stdin.take().unwrap();
        let _ = held.write_all(b"garbage that is no code");
        let output = finish(child);
        drop(held);
        let seen = stub.finish().expect("a request");
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        assert_eq!(seen.json()["code"], code.as_str());
        assert_code_nowhere(&env, &code, &[&output]);
    }

    /// C3 to C7: input that is no code is exit 1 before the lock and before any request, and no
    /// message holds the input or the path.
    #[test]
    fn input_that_is_no_code_is_exit_1_before_the_lock() {
        let env = Env::new();
        env.fingerprint();
        let marker = format!("marker{}", &hex(&shared_crypto::random_32().unwrap())[..12]);
        let valid = code();
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(valid.as_str().as_bytes());
        let long = format!("{}{}", valid.as_str(), " ".repeat(300));
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (Vec::new(), "no registration code on standard input"),
            (
                b"  \r\n\t".to_vec(),
                "no registration code on standard input",
            ),
            (
                marker.clone().into_bytes(),
                "the registration code lacks its version prefix",
            ),
            (
                format!("ngr1-{marker}").into_bytes(),
                "the registration code has the wrong length",
            ),
            (long.into_bytes(), "longer than 256 bytes"),
            (vec![0xFF, 0xFE, 0xFD], "not UTF-8"),
            (with_bom, "byte order mark"),
        ];
        for (input, message) in cases {
            let stub = silent();
            let output = register(&env, &stub, KEY, &text(&input), &[]);
            let output = if std::str::from_utf8(&input).is_err() {
                let mut command = env.command();
                command.args(register_args(stub.port, KEY));
                run_with(command, &input)
            } else {
                output
            };
            assert_exit(&output, 1, "");
            let stderr = text(&output.stderr);
            assert!(stderr.contains(message), "{message}: {stderr}");
            assert!(!stderr.contains(&marker), "{stderr}");
            assert!(stub.finish().is_none());
            assert!(
                !env.registration_dir().exists(),
                "the lock step was not reached"
            );
        }
        // C7: a file that is missing, and a directory.
        for path in [
            env.home.path().join(format!("{marker}-none")),
            env.home.path().to_owned(),
        ] {
            let stub = silent();
            let mut command = env.command();
            command
                .args(register_args(stub.port, KEY))
                .arg("--code-file")
                .arg(&path);
            let output = run_with(command, b"");
            let stderr = text(&output.stderr);
            assert_eq!(output.status.code(), Some(1), "{stderr}");
            assert!(
                stderr.contains("cannot read the file given with --code-file: "),
                "{stderr}"
            );
            assert!(!stderr.contains(&marker), "{stderr}");
            assert!(
                !stderr.contains(env.home.path().to_str().unwrap()),
                "{stderr}"
            );
            assert!(stub.finish().is_none());
        }
    }

    /// C8: a standard input that never ends makes the command wait, and it holds no lock.
    #[test]
    fn a_standard_input_that_never_ends_makes_the_command_wait() {
        let env = Env::new();
        env.fingerprint();
        let stub = silent();
        let mut child = env
            .command()
            .args(register_args(stub.port, KEY))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let held = child.stdin.take().unwrap();
        std::thread::sleep(Duration::from_millis(700));
        assert!(
            child.try_wait().unwrap().is_none(),
            "the command waits for its input"
        );
        // No lock is held and no directory made while it waits.
        assert!(!env.registration_dir().exists());
        child.kill().unwrap();
        let _ = child.wait();
        drop(held);
        assert!(stub.finish().is_none());
    }

    /// L2: a second registration finds the lock held, exits 3 and sends nothing. L3: when the
    /// first ends, its lock goes with it.
    #[test]
    fn a_second_registration_while_one_runs_is_exit_3_and_sends_nothing() {
        let env = Env::new();
        env.fingerprint();
        let server = new_server();
        let code = code();
        let (reached_tx, reached_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let held = {
            let server = server.clone();
            Stub::start(move |seen, stream| {
                reached_tx.send(()).unwrap();
                go_rx.recv_timeout(LIMIT).unwrap();
                let _ = stream.write_all(&created(&answer(seen, &server)));
            })
        };
        let mut command = env.command();
        command.args(register_args(held.port, &key_id(&server)));
        let first = spawn_with(command, code.as_str().as_bytes());
        reached_rx
            .recv_timeout(LIMIT)
            .expect("the first is at the server");

        let other = silent();
        let second = register(&env, &other, &key_id(&server), code.as_str(), &[]);
        assert_exit(&second, 3, "another registration is running");
        assert!(other.finish().is_none());

        go_tx.send(()).unwrap();
        let first = finish(first);
        held.finish().expect("the first's request");
        assert_eq!(first.status.code(), Some(0), "{}", text(&first.stderr));
        assert!(env.record_path().is_file());
        // L3: the next one takes the lock, and sees the record.
        let third = silent();
        let output = register(&env, &third, &key_id(&server), code.as_str(), &[]);
        assert_exit(&output, 5, "this agent already has a registration record");
        assert!(third.finish().is_none());
    }

    /// L5: a directory that is writable by its group or others, and a file where it belongs, are
    /// exit 3 with nothing sent.
    #[test]
    fn an_unusable_registration_directory_is_exit_3_and_sends_nothing() {
        let env = Env::new();
        env.fingerprint();
        let dir = env.registration_dir();
        // A file stands where the directory belongs.
        std::fs::write(&dir, b"a file").unwrap();
        let stub = silent();
        let output = register(&env, &stub, KEY, code().as_str(), &[]);
        assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
        assert!(stub.finish().is_none());
        std::fs::remove_file(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
            let stub = silent();
            let output = register(&env, &stub, KEY, code().as_str(), &[]);
            assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
            assert!(stub.finish().is_none());
            assert!(!dir.join("registration.lock").exists());
        }
    }

    /// R2: a record that exists, whatever it holds, is exit 5; nothing is sent and the file is
    /// byte-identical. R3: with `--replace` the new record replaces it and `replaced` is true.
    #[test]
    fn an_existing_record_is_kept_unless_replace_is_given() {
        let env = Env::new();
        env.fingerprint();
        let server = new_server();
        std::fs::create_dir_all(env.registration_dir()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                env.registration_dir(),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let old = b"something that is no record at all".to_vec();
        std::fs::write(env.record_path(), &old).unwrap();

        let code1 = code();
        let stub = silent();
        let output = register(&env, &stub, &key_id(&server), code1.as_str(), &[]);
        assert_exit(&output, 5, "this agent already has a registration record");
        assert!(stub.finish().is_none());
        assert_eq!(std::fs::read(env.record_path()).unwrap(), old);

        let code2 = code();
        let stub = honest(&server, |_| {});
        let output = register(
            &env,
            &stub,
            &key_id(&server),
            code2.as_str(),
            &["--replace"],
        );
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        stub.finish().expect("a request");
        let printed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(printed["replaced"], true);
        assert_eq!(Record::read(&env.record_path()).unwrap().agent_id, AGENT_ID);
        assert_code_nowhere(&env, &code2, &[&output]);
    }

    /// R4: a directory or a symbolic link where the record belongs is exit 3 before anything is
    /// sent, with or without `--replace`.
    #[test]
    fn a_record_place_that_is_not_a_regular_file_is_exit_3() {
        let env = Env::new();
        env.fingerprint();
        std::fs::create_dir_all(env.registration_dir()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                env.registration_dir(),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        std::fs::create_dir(env.record_path()).unwrap();
        for extra in [&[][..], &["--replace"]] {
            let stub = silent();
            let output = register(&env, &stub, KEY, code().as_str(), extra);
            assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
            assert!(stub.finish().is_none());
        }
        #[cfg(unix)]
        {
            std::fs::remove_dir(env.record_path()).unwrap();
            let target = env.home.path().join("target");
            std::fs::write(&target, b"untouched").unwrap();
            std::os::unix::fs::symlink(&target, env.record_path()).unwrap();
            for extra in [&[][..], &["--replace"]] {
                let stub = silent();
                let output = register(&env, &stub, KEY, code().as_str(), extra);
                assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
                assert!(stub.finish().is_none());
            }
            assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        }
    }

    /// R5: a stale temp is removed; one that cannot be removed is exit 3 before anything is sent.
    #[test]
    fn a_stale_temp_is_removed_and_one_that_cannot_be_is_exit_3() {
        let env = Env::new();
        env.fingerprint();
        let server = new_server();
        std::fs::create_dir_all(env.registration_dir()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                env.registration_dir(),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let temp = env.registration_dir().join(TEMP_FILE);
        std::fs::create_dir(&temp).unwrap();
        std::fs::write(temp.join("x"), b"x").unwrap();
        let stub = silent();
        let output = register(&env, &stub, &key_id(&server), code().as_str(), &[]);
        assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
        assert!(text(&output.stderr).contains("nothing was sent"));
        assert!(stub.finish().is_none());
        std::fs::remove_dir_all(&temp).unwrap();

        std::fs::write(&temp, b"stale").unwrap();
        let stub = honest(&server, |_| {});
        let output = register(&env, &stub, &key_id(&server), code().as_str(), &[]);
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        stub.finish();
        assert!(!temp.exists());
    }

    /// H1: a refused connection is exit 3, "nothing was sent"; the record is as it was.
    #[test]
    fn a_refused_connection_is_exit_3_with_nothing_sent() {
        let env = Env::new();
        env.fingerprint();
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut command = env.command();
        command.args(register_args(port, KEY));
        let output = run_with(command, code().as_str().as_bytes());
        assert_exit(&output, 3, "cannot connect to the server");
        assert!(text(&output.stderr).contains("nothing was sent"));
        assert_nothing_stored(&env);
    }

    const UNKNOWN: &str = "outcome unknown: if the same code is then refused, issue a new one";

    /// H3 and H4: a reset before the head, a head over the limits, an answer that is not HTTP and
    /// a 201 whose body is cut are exit 3 with the outcome unknown.
    #[test]
    fn an_answer_that_does_not_complete_is_exit_3_with_the_outcome_unknown() {
        let env = Env::new();
        env.fingerprint();
        let mut many = b"HTTP/1.1 403 X\r\n".to_vec();
        for i in 0..70 {
            many.extend_from_slice(format!("X-{i}: a\r\n").as_bytes());
        }
        many.extend_from_slice(b"\r\n");
        let mut big = b"HTTP/1.1 403 X\r\nX-Pad: ".to_vec();
        big.extend(std::iter::repeat_n(b'a', 20 * 1024));
        big.extend_from_slice(b"\r\n\r\n");
        let stubs = [
            Stub::start(|_, stream| {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }),
            Stub::raw(many),
            Stub::raw(big),
            Stub::raw(b"SSH-2.0-OpenSSH_9.6\r\n\r\n".to_vec()),
            Stub::raw(b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 400\r\n\r\n{\"agentId\"".to_vec()),
        ];
        for stub in stubs {
            let output = register(&env, &stub, KEY, code().as_str(), &[]);
            assert_exit(&output, 3, UNKNOWN);
            assert!(stub.finish().is_some());
            assert_nothing_stored(&env);
        }
    }

    /// T: each status. The body of a refusal is never printed, and `Location` is not.
    #[test]
    fn each_status_has_its_exit_and_prints_nothing_the_server_said() {
        let env = Env::new();
        env.fingerprint();
        let said = "server-said-this-text";
        let body = said.as_bytes();
        let cases: Vec<(u16, Vec<&str>, i32, &str)> = vec![
            (
                403,
                vec![],
                1,
                "refused: the code is wrong, expired, used or issued for another key, or --server-key is not this server's",
            ),
            (429, vec!["Retry-After: 17"], 1, "wait 17 s"),
            (
                429,
                vec!["Retry-After: 61"],
                1,
                "refused: the code is wrong",
            ),
            (
                429,
                vec!["Retry-After: soon"],
                1,
                "refused: the code is wrong",
            ),
            (400, vec![], 1, "unexpected answer (status 400)"),
            (404, vec![], 1, "unexpected answer (status 404)"),
            (421, vec![], 1, "--server is not the server's own origin"),
            (
                307,
                vec!["Location: http://secret-location.invalid/"],
                1,
                "redirect, not followed",
            ),
            (
                301,
                vec!["Location: http://secret-location.invalid/"],
                1,
                "redirect, not followed",
            ),
            (408, vec![], 3, UNKNOWN),
            (500, vec![], 3, UNKNOWN),
            (503, vec![], 3, UNKNOWN),
            (200, vec![], 1, "unexpected answer (status 200)"),
            (204, vec![], 1, "unexpected answer (status 204)"),
        ];
        for (status, headers, exit, message) in cases {
            let stub = Stub::raw(http(status, &headers, body));
            let output = register(&env, &stub, KEY, code().as_str(), &[]);
            assert_exit(&output, exit, "");
            let stderr = text(&output.stderr);
            assert!(stderr.contains(message), "{status}: {stderr}");
            assert!(!stderr.contains(said), "{status}: {stderr}");
            assert!(!stderr.contains("secret-location"), "{status}: {stderr}");
            assert!(stub.finish().is_some());
            assert_nothing_stored(&env);
        }
    }

    /// V1: a 201 that is not a registration answer is exit 1; nothing is stored.
    #[test]
    fn a_201_that_is_not_an_answer_is_exit_1() {
        let env = Env::new();
        env.fingerprint();
        let server = new_server();
        let json = ["Content-Type: application/json"];
        let wrong_type = {
            let server = server.clone();
            Stub::start(move |seen, stream| {
                let body = answer(seen, &server).to_string();
                let _ =
                    stream.write_all(&http(201, &["Content-Type: text/plain"], body.as_bytes()));
            })
        };
        let mut oversize = serde_json::json!({ "pad": "a".repeat(5000) }).to_string();
        oversize.push(' ');
        let stubs = [
            wrong_type,
            Stub::raw(http(201, &json, b"")),
            Stub::raw(http(201, &json, b"[]")),
            Stub::raw(http(201, &json, b"{\"agentId\":\"x\"}")),
            Stub::raw(http(201, &json, oversize.as_bytes())),
            honest(&server, |v| {
                v["extra"] = "x".into();
            }),
            honest(&server, |v| {
                v.as_object_mut().unwrap().remove("signature");
            }),
            honest(&server, |v| v["allowRemote"] = "no".into()),
        ];
        for stub in stubs {
            let output = register(&env, &stub, &key_id(&server), code().as_str(), &[]);
            assert_exit(&output, 1, "not a registration answer");
            assert!(stub.finish().is_some());
            assert_nothing_stored(&env);
        }
    }

    /// V2, V4, V5, V6: a field that breaks its rule is exit 1.
    #[test]
    fn a_201_with_a_field_that_breaks_its_rule_is_exit_1() {
        let env = Env::new();
        env.fingerprint();
        let server = new_server();
        let id = key_id(&server);
        type Change = Box<dyn FnOnce(&mut serde_json::Value) + Send>;
        let cases: Vec<(Change, &str)> = vec![
            (
                Box::new(|v| v["instructionKey"] = "not a key".into()),
                "instruction key is not valid",
            ),
            (
                Box::new({
                    let id = id.to_uppercase();
                    move |v| v["instructionKeyId"] = id.into()
                }),
                "instruction key ID is not valid",
            ),
            (
                Box::new(|v| v["agentId"] = AGENT_ID.to_uppercase().into()),
                "agent ID is not valid",
            ),
            (
                Box::new(|v| v["ownerId"] = "".into()),
                "owner ID is not valid",
            ),
            (
                Box::new(|v| v["preset"] = "other".into()),
                "preset is not valid",
            ),
            (
                Box::new(|v| v["signature"] = "AAAA".into()),
                "signature is not valid",
            ),
        ];
        for (change, message) in cases {
            let stub = honest(&server, change);
            let output = register(&env, &stub, &id, code().as_str(), &[]);
            assert_exit(&output, 1, "");
            assert!(
                text(&output.stderr).contains(message),
                "{}",
                text(&output.stderr)
            );
            assert!(stub.finish().is_some());
            assert_nothing_stored(&env);
        }
    }

    /// V3 and K4: an answerer that returns 201 with another key is exit 6, and the local
    /// fingerprint is printed.
    #[test]
    fn an_answer_with_another_key_is_exit_6() {
        let env = Env::new();
        env.fingerprint();
        let operator_given = new_server();
        let impostor = new_server();
        let stub = honest(&impostor, |_| {});
        let output = register(&env, &stub, &key_id(&operator_given), code().as_str(), &[]);
        assert_exit(&output, 6, "");
        let stderr = text(&output.stderr);
        assert!(stderr.contains(&key_id(&impostor)), "{stderr}");
        assert!(stub.finish().is_some());
        assert_nothing_stored(&env);
    }

    /// V7: a signature that does not verify is exit 6; a K3 server key that is well-formed but
    /// not this server's gets that server's 403 (the end-to-end test in the server's tests).
    #[test]
    fn a_signature_that_does_not_verify_is_exit_6() {
        let env = Env::new();
        env.fingerprint();
        let server = new_server();
        for change in [
            Box::new(|v: &mut serde_json::Value| v["ownerId"] = "someone-else".into())
                as Box<dyn FnOnce(&mut serde_json::Value) + Send>,
            Box::new(|v| v["allowRemote"] = true.into()),
            Box::new(|v| v["preset"] = "shared".into()),
            Box::new(|v| v["signature"] = STANDARD.encode([0u8; 64]).into()),
        ] {
            let stub = honest(&server, change);
            let output = register(&env, &stub, &key_id(&server), code().as_str(), &[]);
            assert_exit(&output, 6, "the answer's signature does not verify");
            assert!(stub.finish().is_some());
            assert_nothing_stored(&env);
        }
    }

    /// P3: a closed standard output does not undo a registration that is stored.
    #[cfg(unix)]
    #[test]
    fn a_closed_standard_output_still_exits_0() {
        let env = Env::new();
        env.fingerprint();
        let server = new_server();
        let (reached_tx, reached_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let stub = {
            let server = server.clone();
            Stub::start(move |seen, stream| {
                reached_tx.send(()).unwrap();
                go_rx.recv_timeout(LIMIT).unwrap();
                let _ = stream.write_all(&created(&answer(seen, &server)));
            })
        };
        let mut command = env.command();
        command.args(register_args(stub.port, &key_id(&server)));
        let mut child = spawn_with(command, code().as_str().as_bytes());
        reached_rx.recv_timeout(LIMIT).unwrap();
        // The reader of standard output goes away before the command prints.
        drop(child.stdout.take());
        go_tx.send(()).unwrap();
        let output = finish(child);
        stub.finish();
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        assert!(Record::read(&env.record_path()).is_ok());
    }

    /// The reader the session library will use takes the record the command wrote, and refuses it
    /// when another field is added.
    #[test]
    fn the_stored_record_is_read_back_strictly() {
        let env = Env::new();
        env.fingerprint();
        let server = new_server();
        let stub = honest(&server, |_| {});
        let output = register(&env, &stub, &key_id(&server), code().as_str(), &[]);
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        stub.finish();
        let bytes = std::fs::read(env.record_path()).unwrap();
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.get("mode").is_none());
        json["mode"] = "user".into();
        let copy = env.home.path().join("copy.json");
        std::fs::write(&copy, json.to_string()).unwrap();
        assert!(Record::read(&copy).is_err());
    }
}

//! The installation file at the three commands that read it (design 6.7, ADR-494): `identity`,
//! `register` and `connect`, each in both modes, as processes. The unit tests of
//! `crates/agent/src/installation/tests.rs` hold the case table of the file itself; these tests
//! show what the commands do with each outcome: the exit status, the fixed message, and that
//! nothing was touched (no state directory, no key, no lock, no connection) before the file was
//! accepted.
//!
//! A debug build reads the file where `NGR_AGENT_INSTALLATION_FILE` points and trusts the user
//! it runs as to own it. A release build ignores the variable and reads the fixed system
//! location, where a test machine has no file: the release test shows that every command exits 1
//! there with the state untouched, and the rest run in debug builds only.

#[cfg(debug_assertions)]
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[path = "support/installation_file.rs"]
mod installation_file;

const KEY_STORE_VAR: &str = "NGR_AGENT_KEY_STORE";
const STATE_DIR_VAR: &str = "NGR_AGENT_STATE_DIR";
const SERVER_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// A value that no message may repeat.
const SECRET: &str = "SECRETVALUE";

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

    /// `ngr-agent` with its state and home in the temporary directories, the plain-file store
    /// selected and the installation file at `file`.
    fn command(&self, file: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ngr-agent"));
        command
            .args(args)
            .env("HOME", self.home.path())
            .env("XDG_STATE_HOME", self.home.path().join("xdg"))
            .env("LOCALAPPDATA", self.home.path().join("local"))
            .env(KEY_STORE_VAR, "file")
            .env(STATE_DIR_VAR, self.state.path())
            .stdin(Stdio::null());
        installation_file::point(&mut command, file);
        command
    }

    /// The directory the installation file is written in: a child of the home, so that it is a
    /// private directory the test owns.
    fn dir(&self) -> PathBuf {
        let dir = self.home.path().join("installation");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Whether any state directory of the commands exists, in the override's root or, for a
    /// release build that ignores it, in the account's own.
    fn touched(&self) -> bool {
        let own = self
            .home
            .path()
            .join(if cfg!(windows) { "local" } else { "xdg" })
            .join("nekoguruma");
        [self.state.path().to_owned(), own].iter().any(|root| {
            ["identity", "registration", "control", "outbox"]
                .iter()
                .any(|sub| root.join(sub).exists())
        })
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The three commands with `mode`, as the argument lists they are started with.
fn commands(mode: &'static str) -> [(&'static str, Vec<&'static str>); 3] {
    [
        ("identity", vec!["identity", "--mode", mode]),
        (
            "register",
            vec!["register", "--mode", mode, "--server-key", SERVER_KEY],
        ),
        ("connect", vec!["connect", "--mode", mode]),
    ]
}

fn assert_refused(output: &Output, name: &str, contains: &str) {
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{name}: {stderr}");
    assert!(
        output.stdout.is_empty(),
        "{name}: stdout {}",
        text(&output.stdout)
    );
    assert!(
        stderr.contains(&format!("ngr-agent: {name}: {contains}")),
        "{name}: expected {contains:?}: {stderr}"
    );
    assert!(
        !stderr.contains(SECRET),
        "{name}: a value is echoed: {stderr}"
    );
}

#[cfg(all(unix, debug_assertions))]
fn make_writable_by_regular_users(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).unwrap();
}

#[cfg(all(windows, debug_assertions))]
fn make_writable_by_regular_users(path: &Path) {
    // BUILTIN\Users gets write access to the file.
    let status = Command::new("icacls")
        .arg(path)
        .arg("/grant")
        .arg("*S-1-5-32-545:(W)")
        .status()
        .unwrap();
    assert!(status.success(), "icacls failed: {status}");
}

/// A valid file for `mode` with `lines` appended or replacing the key they start with.
#[cfg(debug_assertions)]
fn file_with(mode: &str, replace: &[(&str, &str)], extra: &[&str]) -> String {
    let mut server = "\"http://127.0.0.1:1\"".to_owned();
    let mut mode_line = mode.to_owned();
    let mut pem = format!("\"\"\"\n{}\"\"\"", installation_file::ROOT_PEM);
    for (key, value) in replace {
        match *key {
            "server_url" => server = (*value).to_owned(),
            "mode" => mode_line = (*value).to_owned(),
            "distribution_root_pem" => pem = (*value).to_owned(),
            other => panic!("{other}"),
        }
    }
    let mut text =
        format!("mode = {mode_line}\nserver_url = {server}\ndistribution_root_pem = {pem}\n");
    for line in extra {
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// A1 to D15, F: what each command does with each file that cannot be used: exit 1, a fixed
/// message that repeats no value, and no state, key, lock or connection touched, in both modes. A
/// file readable and refused in user mode too (W: the user mode's account cannot change what its
/// own agent installed) is one of the cases.
#[cfg(debug_assertions)]
#[test]
fn a_file_that_cannot_be_used_is_exit_1_with_nothing_touched() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let env = Env::new();
    let dir = env.dir();
    // Each case: its name, the message each command starts with after `ngr-agent: <name>: `, and
    // the file it plants (None: no file).
    type Plant = Box<dyn Fn(&str) -> Option<Vec<u8>>>;
    let cases: Vec<(&str, &str, Plant, bool)> = vec![
        (
            "absent",
            "there is no installation file",
            Box::new(|_| None),
            false,
        ),
        (
            "unparsable",
            "the installation file is not valid TOML (line 2)",
            Box::new(|mode| {
                Some(format!("mode = \"{mode}\"\nserver_url = {SECRET}\n").into_bytes())
            }),
            false,
        ),
        (
            "not UTF-8",
            "the installation file is not UTF-8",
            Box::new(|mode| {
                let mut bytes = installation_file::text(mode, "http://127.0.0.1:1").into_bytes();
                bytes.extend_from_slice(b"# \xff\xfe\n");
                Some(bytes)
            }),
            false,
        ),
        (
            "too large",
            "the installation file is larger than 65536 bytes",
            Box::new(|mode| {
                let mut text = installation_file::text(mode, "http://127.0.0.1:1");
                text.push_str(&format!("# {}\n", "x".repeat(70_000)));
                Some(text.into_bytes())
            }),
            false,
        ),
        (
            "empty",
            "the installation file lacks mode",
            Box::new(|_| Some(Vec::new())),
            false,
        ),
        (
            "no server_url",
            "the installation file lacks server_url",
            Box::new(|mode| {
                Some(
                    format!(
                        "mode = \"{mode}\"\ndistribution_root_pem = \"\"\"\n{}\"\"\"\n",
                        installation_file::ROOT_PEM
                    )
                    .into_bytes(),
                )
            }),
            false,
        ),
        (
            "no distribution_root_pem",
            "the installation file lacks distribution_root_pem",
            Box::new(|mode| {
                Some(
                    format!("mode = \"{mode}\"\nserver_url = \"http://127.0.0.1:1\"\n")
                        .into_bytes(),
                )
            }),
            false,
        ),
        (
            "unknown key",
            "the installation file has an unknown key: moed",
            Box::new(|mode| {
                Some(file_with(&format!("\"{mode}\""), &[], &["moed = 1"]).into_bytes())
            }),
            false,
        ),
        (
            "invalid server_url",
            "the installation file's server_url must be http://127.0.0.1",
            Box::new(|mode| {
                Some(
                    file_with(
                        &format!("\"{mode}\""),
                        &[("server_url", &format!("\"http://{SECRET}.example\""))],
                        &[],
                    )
                    .into_bytes(),
                )
            }),
            false,
        ),
        (
            "invalid pem",
            "the installation file's distribution_root_pem must be one P-256 public key",
            Box::new(|mode| {
                Some(
                    file_with(
                        &format!("\"{mode}\""),
                        &[("distribution_root_pem", &format!("\"{SECRET}\""))],
                        &[],
                    )
                    .into_bytes(),
                )
            }),
            false,
        ),
        (
            "standard profile",
            "the installation file's profile standard is not supported yet",
            Box::new(|mode| {
                Some(
                    file_with(&format!("\"{mode}\""), &[], &["profile = \"standard\""])
                        .into_bytes(),
                )
            }),
            false,
        ),
        (
            "relative lock_dir",
            "the installation file's lock_dir must be an absolute path",
            Box::new(|mode| {
                Some(
                    file_with(
                        &format!("\"{mode}\""),
                        &[],
                        &[&format!("lock_dir = \"{SECRET}\"")],
                    )
                    .into_bytes(),
                )
            }),
            false,
        ),
        (
            "writable by regular users",
            "the installation file fails the writability check of design 7.2",
            Box::new(|mode| Some(installation_file::text(mode, "http://127.0.0.1:1").into_bytes())),
            true,
        ),
    ];
    for (mode, other) in [("device", "user"), ("user", "device")] {
        let _ = other;
        let mut extra: Vec<(&str, &str, Plant, bool)> = Vec::new();
        if mode == "device" {
            extra.push((
                "explicit startup in device mode",
                "the installation file's startup must not be \"explicit\" in device mode",
                Box::new(|mode| {
                    Some(
                        file_with(&format!("\"{mode}\""), &[], &["startup = \"explicit\""])
                            .into_bytes(),
                    )
                }),
                false,
            ));
        }
        for (name, message, plant, writable) in cases.iter().chain(extra.iter()) {
            for (command_name, args) in commands(mode) {
                let file = dir.join("agent.toml");
                let _ = std::fs::remove_file(&file);
                if let Some(bytes) = plant(mode) {
                    std::fs::write(&file, bytes).unwrap();
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))
                            .unwrap();
                    }
                    if *writable {
                        make_writable_by_regular_users(&file);
                    }
                }
                let output = env.command(&file, &args).output().unwrap();
                let label = format!("{name} ({mode})");
                assert_refused(&output, command_name, message);
                assert!(!env.touched(), "{label}: {command_name} touched the state");
            }
        }
    }
    assert!(listener.accept().is_err(), "a command connected");
}

/// E1 to E4 (ADR-494): `--mode` must equal the installed mode, whichever way they differ; the
/// message names the two modes, which are fixed words, and nothing is touched.
#[cfg(debug_assertions)]
#[test]
fn a_mode_other_than_the_installed_one_is_exit_1_with_nothing_touched() {
    for (installed, asked) in [("device", "user"), ("user", "device")] {
        let env = Env::new();
        let file = installation_file::write(
            &env.dir(),
            &installation_file::text(installed, "http://127.0.0.1:1"),
        );
        for (name, args) in commands(asked) {
            let output = env.command(&file, &args).output().unwrap();
            assert_refused(
                &output,
                name,
                &format!("--mode {asked} is not the installed mode ({installed})"),
            );
            assert!(!env.touched(), "{name} touched the state");
        }
        // The mode the file names goes on: `identity` creates its key.
        let output = env
            .command(&file, &["identity", "--mode", installed])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    }
}

/// F5: with the location override a debug build trusts its own user as the file's owner; the
/// same file is read in both modes and goes on to the key store, the registration record and
/// the key.
#[cfg(debug_assertions)]
#[test]
fn a_valid_file_lets_each_command_go_on_in_both_modes() {
    for mode in ["device", "user"] {
        let env = Env::new();
        let file = installation_file::write(
            &env.dir(),
            &installation_file::text(mode, "http://127.0.0.1:1"),
        );
        // `connect` finds no record, before the key store is touched.
        let output = env
            .command(&file, &["connect", "--mode", mode])
            .output()
            .unwrap();
        assert_refused(&output, "connect", "not registered");
        assert!(!env.state.path().join("identity").exists());
        // `identity` creates the key.
        let output = env
            .command(&file, &["identity", "--mode", mode])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        // `register` needs a code: standard input holds none, and it ends at that, after the
        // installation file and the key.
        let output = env
            .command(
                &file,
                &["register", "--mode", mode, "--server-key", SERVER_KEY],
            )
            .output()
            .unwrap();
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains("register: no registration code"),
            "{stderr}"
        );
    }
}

/// G (ADR-494): `register --server` is optional and must be the installed origin in any spelling
/// the origin parser unifies; another origin is exit 1 before the key, a lock or a connection.
/// Without `--server` the registration goes to the installed server.
#[cfg(debug_assertions)]
#[test]
fn register_takes_the_server_from_the_installation_file() {
    let installed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    installed.set_nonblocking(true).unwrap();
    let other = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    other.set_nonblocking(true).unwrap();
    let port = installed.local_addr().unwrap().port();
    let other_port = other.local_addr().unwrap().port();
    let env = Env::new();
    let file = installation_file::write(
        &env.dir(),
        &installation_file::text("user", &format!("http://127.0.0.1:{port}")),
    );
    let base = ["register", "--mode", "user", "--server-key", SERVER_KEY];
    // Another server, and the default port of the installed host: exit 1, nothing touched.
    for server in [
        format!("http://127.0.0.1:{other_port}"),
        "http://127.0.0.1".to_owned(),
    ] {
        let output = env
            .command(&file, &[&base[..], &["--server", &server]].concat())
            .output()
            .unwrap();
        assert_refused(
            &output,
            "register",
            "--server is not the server the installation file names",
        );
        let stderr = text(&output.stderr);
        assert!(!stderr.contains(&port.to_string()) && !stderr.contains(&other_port.to_string()));
        assert!(!env.touched());
    }
    assert!(installed.accept().is_err() && other.accept().is_err());
    // The key must exist for the registration to reach the server.
    let output = env
        .command(&file, &["identity", "--mode", "user"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    // The installed origin, spelled another way, and no `--server` at all: both connect to the
    // installed server, which here closes the connection at once (the outcome is unknown, exit
    // 3, and the local record is as it was).
    for extra in [
        vec!["--server".to_owned(), format!("HTTP://127.0.0.1:{port}/")],
        vec![],
    ] {
        installed.set_nonblocking(false).unwrap();
        let listener = installed.try_clone().unwrap();
        let (accepted, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
                let _ = accepted.send(());
            }
        });
        let mut command = env.command(&file, &[&base[..], &["--replace"]].concat());
        command.args(&extra).stdin(Stdio::piped());
        let mut child = command.spawn().unwrap();
        {
            use std::io::Write;
            let code = shared_crypto::registration::RegistrationCode::generate().unwrap();
            let mut input = child.stdin.take().unwrap();
            let _ = input.write_all(code.as_str().as_bytes());
        }
        let output = child.wait_with_output().unwrap();
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(3), "{extra:?}: {stderr}");
        assert!(
            seen.recv_timeout(std::time::Duration::from_secs(10))
                .is_ok(),
            "{extra:?}: the installed server was not contacted: {stderr}"
        );
    }
    assert!(other.accept().is_err(), "the other server was contacted");
}

/// F1 to F3, a release build: the location override is ignored, so every command in both modes
/// exits 1 for want of a file at the fixed system location, with the state untouched, even when
/// a valid file stands where the variable points.
#[cfg(not(debug_assertions))]
#[test]
fn a_release_build_ignores_the_location_override() {
    for mode in ["device", "user"] {
        let env = Env::new();
        let file = installation_file::write(
            &env.dir(),
            &installation_file::text(mode, "http://127.0.0.1:1"),
        );
        for (name, args) in commands(mode) {
            let output = env.command(&file, &args).output().unwrap();
            assert_refused(&output, name, "there is no installation file");
            assert!(!env.touched(), "{name} touched the state");
        }
    }
}

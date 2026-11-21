//! `ngr-agent provision-key` (design 6.4, ADR-498) against a fake `systemd-creds`: a script first on
//! `PATH` that "encrypts" by base64 and "decrypts" by the reverse, records its arguments and
//! environment, and misbehaves on request. Real systemd is not needed.
//!
//! A debug build moves the target directory with `NGR_AGENT_CREDSTORE_DIR` and so lifts the root
//! check, which is how these tests run as an ordinary user; a release build has neither, and its
//! one test here checks that it refuses a caller who is not root and ignores the variable.

#![cfg(target_os = "linux")]

#[cfg(debug_assertions)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[path = "support/installation_file.rs"]
mod installation_file;

const KEY_STORE_VAR: &str = "NGR_AGENT_KEY_STORE";

/// The fake tool. The `fake` directory beside its own (it runs with a cleared environment, so
/// it finds the directory from `$0`): `mode` picks how it misbehaves, and every call appends its
/// arguments to `argv` and its environment to `env`.
#[cfg(debug_assertions)]
const FAKE: &str = r#"#!/bin/sh
dir="$(dirname "$0")/../fake"
printf '%s\n' "$*" >> "$dir/argv"
env >> "$dir/env"
mode=$(cat "$dir/mode" 2>/dev/null || echo ok)
case "$1:$mode" in
  encrypt:fail) echo "fake: encrypt failed" >&2; exit 1 ;;
  encrypt:empty) exit 0 ;;
  encrypt:big) head -c 1000000 /dev/zero | tr '\0' a ;;
  encrypt:bg) (sleep 4 &); base64 -w0 ;;
  encrypt:escape) (setsid sh -c 'echo $$ > "$0"; exec sleep 3' "$dir/escaped.pid" </dev/null 2>/dev/null &); base64 -w0 ;;
  encrypt:pid) echo $$ > "$dir/tool.pid"; exec sleep 30 ;;
  encrypt:slow) sleep 0.4; base64 -w0 ;;
  decrypt:decrypt-fail) echo "fake: decrypt failed" >&2; exit 1 ;;
  decrypt:other) cat "$dir/other.der" ;;
  encrypt:*) base64 -w0 ;;
  decrypt:*) base64 -d ;;
esac
"#;

#[cfg(not(debug_assertions))]
fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// A release build refuses a caller who is not root (and as root would write to `/etc`, so the
/// test does not run then), and does not read `NGR_AGENT_CREDSTORE_DIR`.
#[cfg(not(debug_assertions))]
#[test]
fn a_release_build_refuses_a_caller_who_is_not_root() {
    if is_root() {
        return;
    }
    let target = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut command = base(home.path());
    command
        .args(["provision-key", "--mode", "device"])
        .env("NGR_AGENT_CREDSTORE_DIR", target.path().join("c"));
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("root"));
    assert!(output.stdout.is_empty());
    assert!(!target.path().join("c").exists());
}

/// A release build checks the closed `--with-key` list before anything else.
#[cfg(not(debug_assertions))]
#[test]
fn a_release_build_refuses_a_value_outside_the_list() {
    let home = tempfile::tempdir().unwrap();
    let output = base(home.path())
        .args(["provision-key", "--mode", "device", "--with-key", "none"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

fn base(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ngr-agent"));
    command
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("xdg"))
        .env_remove(KEY_STORE_VAR)
        .env_remove("NGR_AGENT_STATE_DIR");
    command
}

#[cfg(debug_assertions)]
mod debug {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Output, Stdio};
    use std::time::{Duration, Instant};

    use super::*;

    const NAME: &str = "nekoguruma-agent-key.device.v1";
    use base64::Engine;

    /// A temporary world: the fake tool, the target directory (not yet made), `TMPDIR` and a home
    /// with the device installation file.
    pub struct Fixture {
        _root: tempfile::TempDir,
        home: PathBuf,
        bin: PathBuf,
        fake: PathBuf,
        target: PathBuf,
        tmp: PathBuf,
    }

    impl Fixture {
        pub fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let make = |name: &str| {
                let dir = root.path().join(name);
                fs::create_dir(&dir).unwrap();
                dir
            };
            let (home, bin, fake, tmp) = (make("home"), make("bin"), make("fake"), make("tmp"));
            let tool = bin.join("systemd-creds");
            fs::write(&tool, FAKE).unwrap();
            fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
            let target = root.path().join("credstore");
            Self {
                _root: root,
                home,
                bin,
                fake,
                target,
                tmp,
            }
        }

        pub fn mode(&self, mode: &str) {
            fs::write(self.fake.join("mode"), mode).unwrap();
        }

        pub fn path(&self) -> String {
            format!("{}:/usr/bin:/bin", self.bin.display())
        }

        /// `provision-key` with `args` and an installation file for `installed`, with the fake
        /// first on `PATH`.
        fn raw(&self, args: &[&str], installed: &str) -> Command {
            let mut command = base(&self.home);
            command
                .arg("provision-key")
                .args(args)
                .env("PATH", self.path())
                .env("TMPDIR", &self.tmp)
                .env("NGR_AGENT_CREDSTORE_DIR", &self.target);
            installation_file::install(&mut command, &self.home, installed, "http://127.0.0.1:1");
            command
        }

        /// `provision-key --mode device` and `args`, for a device installation.
        pub fn command(&self, args: &[&str]) -> Command {
            let mut all = vec!["--mode", "device"];
            all.extend_from_slice(args);
            self.raw(&all, "device")
        }

        pub fn run(&self, args: &[&str]) -> Output {
            self.command(args).output().unwrap()
        }

        pub fn env_log(&self) -> String {
            fs::read_to_string(self.fake.join("env")).unwrap_or_default()
        }

        pub fn fake_dir(&self) -> &Path {
            &self.fake
        }

        pub fn target(&self) -> &Path {
            &self.target
        }

        pub fn assert_nothing_in_place(&self) {
            assert_nothing_in_place(self);
        }

        pub fn final_path(&self) -> PathBuf {
            self.target.join(NAME)
        }

        pub fn calls(&self) -> Vec<String> {
            fs::read_to_string(self.fake.join("argv"))
                .map(|text| text.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        }

        pub fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.target)
                .map(|entries| {
                    entries
                        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        }
    }

    fn code(output: &Output) -> Option<i32> {
        output.status.code()
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// The three-field JSON line of a created key.
    fn printed(output: &Output) -> (String, String) {
        assert_eq!(code(output), Some(0), "{}", stderr(output));
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 3, "{json}");
        assert_eq!(json["created"], true);
        (
            json["public_key_pem"].as_str().unwrap().to_owned(),
            json["fingerprint"].as_str().unwrap().to_owned(),
        )
    }

    /// The key the fake sealed: the file holds its base64.
    fn opened(fixture: &Fixture) -> Vec<u8> {
        let text = fs::read(fixture.final_path()).unwrap();
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .unwrap()
    }

    /// P1, P10: nothing at the final name afterwards: no file and no temporary one.
    fn assert_nothing_in_place(fixture: &Fixture) {
        let names = fixture.names();
        assert!(
            names
                .iter()
                .all(|name| name != NAME && !name.ends_with(".tmp")),
            "{names:?}"
        );
    }

    /// P12, P8, P2: a key is created, printed and written 0600 into a target directory the command
    /// made 0700; no temporary file stays; the printed fingerprint is the written key's, which
    /// `identity` finds through the fake-decrypted file; both tool calls carry the closed
    /// arguments.
    #[test]
    fn the_happy_path_creates_the_credential_once() {
        let fixture = Fixture::new();
        let output = fixture.run(&[]);
        let (_, fingerprint) = printed(&output);
        assert!(stderr(&output).contains("restarted"));
        assert_eq!(fixture.names(), [NAME]);
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&fixture.final_path()), 0o600);
        assert_eq!(mode(&fixture.target), 0o700);
        assert_eq!(
            fixture.calls(),
            [
                format!("encrypt --name={NAME} --with-key=auto --tpm2-pcrs= - -"),
                format!("decrypt --name={NAME} - -"),
            ]
        );

        // What systemd would hand the unit: the decrypted file in a credentials directory.
        let creds = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        fs::write(creds.path().join(NAME), opened(&fixture)).unwrap();
        let mut identity = base(&fixture.home);
        identity
            .args(["identity", "--mode", "device"])
            .env(KEY_STORE_VAR, "systemd-credential")
            .env("CREDENTIALS_DIRECTORY", creds.path())
            .env("NGR_AGENT_STATE_DIR", state.path());
        installation_file::install(&mut identity, &fixture.home, "device", "http://127.0.0.1:1");
        let identity = identity.output().unwrap();
        assert_eq!(code(&identity), Some(0), "{}", stderr(&identity));
        let json: serde_json::Value = serde_json::from_slice(&identity.stdout).unwrap();
        assert_eq!(json["fingerprint"], fingerprint.as_str());
        assert_eq!(json["created"], false);

        // A second run finds the credential and stops before the tool.
        let calls = fixture.calls().len();
        let again = fixture.run(&[]);
        assert_eq!(code(&again), Some(5), "{}", stderr(&again));
        assert!(again.stdout.is_empty());
        assert_eq!(fixture.calls().len(), calls);
    }

    /// The closed `--with-key` values reach the tool as they are.
    #[test]
    fn every_with_key_value_reaches_the_tool() {
        for value in ["auto", "host", "tpm2", "host+tpm2"] {
            let fixture = Fixture::new();
            printed(&fixture.run(&["--with-key", value]));
            assert!(
                fixture.calls()[0].contains(&format!("--with-key={value} ")),
                "{:?}",
                fixture.calls()
            );
        }
    }

    /// P1: anything at the final name, a dangling symlink and an empty file included, exits 5
    /// before a key is generated or the tool is started, and is left as it is.
    #[test]
    fn anything_at_the_final_name_exits_with_5_and_the_tool_never_runs() {
        for plant in ["file", "empty", "dangling", "directory"] {
            let fixture = Fixture::new();
            fs::create_dir(&fixture.target).unwrap();
            fs::set_permissions(&fixture.target, fs::Permissions::from_mode(0o700)).unwrap();
            let path = fixture.final_path();
            match plant {
                "file" => fs::write(&path, b"sealed").unwrap(),
                "empty" => fs::write(&path, b"").unwrap(),
                "dangling" => std::os::unix::fs::symlink("/nonexistent/target", &path).unwrap(),
                _ => fs::create_dir(&path).unwrap(),
            }
            let output = fixture.run(&[]);
            assert_eq!(code(&output), Some(5), "{plant}: {}", stderr(&output));
            assert!(output.stdout.is_empty());
            assert!(fixture.calls().is_empty(), "{plant}");
            assert_eq!(fixture.names(), [NAME], "{plant}");
            match plant {
                "file" => assert_eq!(fs::read(&path).unwrap(), b"sealed"),
                "empty" => assert_eq!(fs::read(&path).unwrap(), b""),
                "dangling" => assert!(fs::symlink_metadata(&path).unwrap().is_symlink()),
                _ => assert!(path.is_dir()),
            }
        }
    }

    /// P8: two runs at once make one credential: one exits 0 and one exits 5, and no temporary
    /// file stays.
    #[test]
    fn two_concurrent_runs_create_one_credential() {
        let fixture = Fixture::new();
        fixture.mode("slow");
        let children: Vec<_> = (0..2)
            .map(|_| {
                fixture
                    .command(&[])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let mut codes: Vec<_> = children
            .into_iter()
            .map(|child| code(&child.wait_with_output().unwrap()).unwrap())
            .collect();
        codes.sort_unstable();
        assert_eq!(codes, [0, 5]);
        assert_eq!(fixture.names(), [NAME]);
    }

    /// P3: no tool on `PATH` exits 1 with the version it needs, before any key is generated or
    /// anything is written.
    #[test]
    fn no_tool_on_the_path_exits_with_1() {
        let fixture = Fixture::new();
        let empty = tempfile::tempdir().unwrap();
        let output = fixture
            .command(&[])
            .env("PATH", empty.path())
            .output()
            .unwrap();
        assert_eq!(code(&output), Some(1), "{}", stderr(&output));
        assert!(stderr(&output).contains("systemd 250 or later"));
        assert!(output.stdout.is_empty());
        assert_nothing_in_place(&fixture);
        assert!(fixture.calls().is_empty());
    }

    /// P9, P10: a tool that fails, prints nothing or too much leaves nothing at the final name;
    /// its stderr reaches the caller; a failing verification exits 3.
    #[test]
    fn a_misbehaving_tool_leaves_nothing_in_place() {
        for (mode, expected, said) in [
            ("fail", 1, Some("fake: encrypt failed")),
            ("empty", 3, None),
            ("big", 3, None),
            ("decrypt-fail", 3, Some("fake: decrypt failed")),
        ] {
            let fixture = Fixture::new();
            fixture.mode(mode);
            let output = fixture.run(&[]);
            assert_eq!(code(&output), Some(expected), "{mode}: {}", stderr(&output));
            assert!(output.stdout.is_empty(), "{mode}");
            if let Some(said) = said {
                assert!(
                    stderr(&output).contains(said),
                    "{mode}: {}",
                    stderr(&output)
                );
            }
            assert_nothing_in_place(&fixture);
        }
    }

    /// P9: a tool that hangs (here a script that starts a `sleep` of its own) is killed at the
    /// bound, a short one injected through the library, and nothing is written.
    #[test]
    fn a_hanging_tool_is_killed_at_the_bound() {
        use agent::identity::WithKey;
        use agent::identity::provision::{Plan, ProvisionError, provision_with};
        let fixture = Fixture::new();
        let hanging = fixture.bin.join("hanging");
        fs::create_dir(&hanging).unwrap();
        let tool = hanging.join("systemd-creds");
        fs::write(&tool, "#!/bin/sh\nsleep 30\necho late\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        // The tool runs with this `PATH` alone, and its script needs `sleep`.
        let search = std::ffi::OsString::from(format!("{}:/usr/bin:/bin", hanging.display()));
        let plan = Plan {
            dir: &fixture.target,
            search_path: &search,
            with_key: WithKey::Auto,
            bound: Duration::from_millis(300),
            root_owned: false,
        };
        let started = Instant::now();
        let error = provision_with(&plan).unwrap_err();
        assert!(matches!(error, ProvisionError::Io(_)), "{error}");
        assert_eq!(error.exit_code(), 3);
        assert!(error.to_string().contains("killed"), "{error}");
        // Well under the tool's own 30 s: the kill ended it, and its child with it.
        assert!(started.elapsed() < Duration::from_secs(15));
        assert_nothing_in_place(&fixture);
    }

    /// P10: a decrypt that returns another key is exit 3, and nothing is linked.
    #[test]
    fn a_verification_that_returns_another_key_exits_with_3() {
        let fixture = Fixture::new();
        fixture.mode("other");
        let other = shared_crypto::SigningKey::generate().unwrap();
        fs::write(fixture.fake.join("other.der"), &*other.to_pkcs8_der()).unwrap();
        let output = fixture.run(&[]);
        assert_eq!(code(&output), Some(3), "{}", stderr(&output));
        assert!(stderr(&output).contains("verification"));
        assert!(output.stdout.is_empty());
        assert_nothing_in_place(&fixture);
        assert_eq!(fixture.calls().len(), 2);
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn files_under(dir: &Path, found: &mut BTreeSet<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_under(&path, found);
            } else {
                found.insert(path);
            }
        }
    }

    /// P11: the key bytes are in no argument and no environment the tool saw, and in no file of
    /// the target directory or `TMPDIR`, raw or in hexadecimal. (The fake's ciphertext is the key's
    /// base64, so the target is searched for the raw forms only.)
    #[test]
    fn the_key_is_in_no_argument_environment_or_file() {
        let fixture = Fixture::new();
        printed(&fixture.run(&[]));
        let der = opened(&fixture);
        let scalar = &der[36..68];
        let base64 = base64::engine::general_purpose::STANDARD.encode(&der);
        let raw: Vec<&[u8]> = vec![&der, scalar];
        let texts = [hex(&der), hex(scalar)];

        for log in ["argv", "env"] {
            let seen = fs::read(fixture.fake.join(log)).unwrap();
            assert!(!seen.is_empty(), "{log}");
            for needle in &raw {
                assert!(!contains(&seen, needle), "{log} holds key bytes");
            }
            for text in texts.iter().chain([&base64]) {
                assert!(
                    !contains(&seen, text.as_bytes()),
                    "{log} holds the key as text"
                );
            }
        }
        let mut files = BTreeSet::new();
        files_under(&fixture.target, &mut files);
        files_under(&fixture.tmp, &mut files);
        assert!(files.contains(&fixture.final_path()));
        for file in files {
            let bytes = fs::read(&file).unwrap();
            for needle in &raw {
                assert!(
                    !contains(&bytes, needle),
                    "{} holds key bytes",
                    file.display()
                );
            }
            for text in &texts {
                assert!(
                    !contains(&bytes, text.as_bytes()),
                    "{} holds the key",
                    file.display()
                );
            }
        }
    }

    /// P8: a temporary file an earlier attempt left is removed.
    #[test]
    fn a_stale_temporary_file_is_removed() {
        let fixture = Fixture::new();
        fs::create_dir(&fixture.target).unwrap();
        fs::set_permissions(&fixture.target, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(fixture.target.join(format!("{NAME}.tmp")), b"half").unwrap();
        printed(&fixture.run(&[]));
        assert_eq!(fixture.names(), [NAME]);
    }

    /// P2: a target directory that its group or others may write, or a symlink, is refused (exit
    /// 1) before the tool runs, and is left as it is.
    #[test]
    fn an_unsafe_target_directory_is_refused() {
        let fixture = Fixture::new();
        fs::create_dir(&fixture.target).unwrap();
        fs::set_permissions(&fixture.target, fs::Permissions::from_mode(0o775)).unwrap();
        let output = fixture.run(&[]);
        assert_eq!(code(&output), Some(1), "{}", stderr(&output));
        assert!(stderr(&output).contains("writable"));
        assert!(fixture.calls().is_empty());
        assert!(fixture.names().is_empty());

        let real = fixture.target.with_file_name("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir(&fixture.target).unwrap();
        std::os::unix::fs::symlink(&real, &fixture.target).unwrap();
        let output = fixture.run(&[]);
        assert_eq!(code(&output), Some(1), "{}", stderr(&output));
        assert!(fixture.calls().is_empty());
        assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
    }

    /// P6, P4: `--mode user`, an installation file for another mode or none, and a `--with-key`
    /// outside the list stop the command before the tool runs: 1, 1, 1 and 2.
    #[test]
    fn what_the_command_cannot_do_stops_it_before_the_tool() {
        let fixture = Fixture::new();
        let output = fixture.raw(&["--mode", "user"], "user").output().unwrap();
        assert_eq!(code(&output), Some(1), "{}", stderr(&output));

        // The installation file is for the user mode.
        let output = fixture.raw(&["--mode", "device"], "user").output().unwrap();
        assert_eq!(code(&output), Some(1), "{}", stderr(&output));

        // No installation file.
        let mut missing = fixture.command(&[]);
        installation_file::point(&mut missing, &fixture.home.join("nothing.toml"));
        let output = missing.output().unwrap();
        assert_eq!(code(&output), Some(1), "{}", stderr(&output));

        let output = fixture.run(&["--with-key", "tpm2-absent"]);
        assert_eq!(code(&output), Some(2), "{}", stderr(&output));
        assert!(stderr(&output).contains("usage: ngr-agent"));

        assert!(fixture.calls().is_empty());
        assert!(!fixture.target.exists());
    }
}

#[cfg(debug_assertions)]
mod hardening {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use agent::identity::WithKey;
    use agent::identity::provision::{Plan, ProvisionError, provision_with};

    use super::debug::Fixture;

    fn plan<'a>(fixture: &'a Fixture, search: &'a std::ffi::OsStr, bound: Duration) -> Plan<'a> {
        Plan {
            dir: fixture.target(),
            search_path: search,
            with_key: WithKey::Auto,
            bound,
            root_owned: false,
        }
    }

    /// A descendant of the tool that stays in its group and holds standard output is killed with
    /// the group once the tool has exited, so the run ends at once and succeeds.
    #[test]
    fn a_descendant_in_the_tools_group_does_not_hold_the_command() {
        let fixture = Fixture::new();
        fixture.mode("bg");
        let search = std::ffi::OsString::from(fixture.path());
        let started = Instant::now();
        provision_with(&plan(&fixture, &search, Duration::from_secs(3))).unwrap();
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// A descendant that left the group (`setsid`) and holds standard output is caught by the
    /// bound: exit 3 within it, and nothing is linked.
    #[test]
    fn a_descendant_outside_the_group_is_caught_by_the_bound() {
        let fixture = Fixture::new();
        fixture.mode("escape");
        let search = std::ffi::OsString::from(fixture.path());
        let started = Instant::now();
        let error =
            provision_with(&plan(&fixture, &search, Duration::from_millis(700))).unwrap_err();
        assert!(matches!(error, ProvisionError::Io(_)), "{error}");
        assert_eq!(error.exit_code(), 3);
        assert!(error.to_string().contains("timeout"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
        fixture.assert_nothing_in_place();
        // The escaped process is the test's to end.
        if let Some(pid) = fs::read_to_string(fixture.fake_dir().join("escaped.pid"))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        {
            // SAFETY: a plain signal to the process the fake recorded.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    /// The release rule for the target directory (`root_owned`): one that another account owns
    /// is refused (exit 1) before the tool runs. As root the directory is given to an unused uid
    /// first; without that power the test has nothing to show and returns.
    #[test]
    fn a_target_directory_of_another_owner_is_refused() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.target()).unwrap();
        fs::set_permissions(fixture.target(), fs::Permissions::from_mode(0o700)).unwrap();
        // SAFETY: geteuid has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            let path = std::ffi::CString::new(fixture.target().to_str().unwrap()).unwrap();
            // SAFETY: a valid NUL-terminated path; the group is left as it is.
            if unsafe { libc::chown(path.as_ptr(), 64_321, u32::MAX) } != 0 {
                return;
            }
        }
        let search = std::ffi::OsString::from(fixture.path());
        let mut plan = plan(&fixture, &search, Duration::from_secs(5));
        plan.root_owned = true;
        let error = provision_with(&plan).unwrap_err();
        assert_eq!(error.exit_code(), 1, "{error}");
        assert!(error.to_string().contains("owned by root"), "{error}");
        assert!(fixture.calls().is_empty());
    }

    /// P2: a regular file where the target directory belongs is refused (exit 1), untouched, and
    /// the tool never runs.
    #[test]
    fn a_file_at_the_target_path_is_refused() {
        let fixture = Fixture::new();
        fs::write(fixture.target(), b"a file").unwrap();
        let output = fixture.run(&[]);
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("not a directory"));
        assert!(fixture.calls().is_empty());
        assert_eq!(fs::read(fixture.target()).unwrap(), b"a file");
    }

    /// A stale temporary file beside a final name that already exists is removed too, and the
    /// answer is still exit 5 with the credential untouched.
    #[test]
    fn a_stale_temporary_file_beside_an_existing_credential_is_removed() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.target()).unwrap();
        fs::set_permissions(fixture.target(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(fixture.final_path(), b"sealed").unwrap();
        let tmp = fixture
            .target()
            .join(format!("{}.tmp", agent::identity::DEVICE_CREDENTIAL_NAME));
        fs::write(&tmp, b"half").unwrap();
        let output = fixture.run(&[]);
        assert_eq!(output.status.code(), Some(5));
        assert!(!tmp.exists());
        assert_eq!(fs::read(fixture.final_path()).unwrap(), b"sealed");
        assert!(fixture.calls().is_empty());
    }

    /// The tool runs with `PATH`, `LANG` and `LC_*` only: nothing else of the caller's
    /// environment reaches it.
    #[test]
    fn the_tool_gets_a_cleared_environment() {
        let fixture = Fixture::new();
        let output = fixture
            .command(&[])
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("SYSTEMD_LOG_LEVEL", "debug")
            .env("SECRET_PROBE", "leak")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        let log = fixture.env_log();
        let names: std::collections::BTreeSet<&str> = log
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .collect();
        for kept in ["PATH", "LANG", "LC_ALL"] {
            assert!(names.contains(kept), "{kept} missing from {names:?}");
        }
        // What a shell adds itself is not the caller's.
        let shell = ["PWD", "SHLVL", "_", "OLDPWD"];
        for name in names {
            assert!(
                ["PATH", "LANG"].contains(&name)
                    || name.starts_with("LC_")
                    || shell.contains(&name),
                "{name} reached the tool"
            );
        }
    }

    fn alive(pid: i32) -> bool {
        // A zombie is gone for this purpose.
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| !stat.contains(") Z"))
    }

    /// The tool dies with the provisioner: the provisioner is killed while the tool runs, and the
    /// tool's pid, which the fake recorded, is gone soon after.
    #[test]
    fn the_tool_dies_when_the_provisioner_is_killed() {
        let fixture = Fixture::new();
        fixture.mode("pid");
        let mut child = fixture
            .command(&[])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let file = fixture.fake_dir().join("tool.pid");
        let started = Instant::now();
        let pid = loop {
            if let Some(pid) = fs::read_to_string(&file)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                break pid;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "no tool pid");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(alive(pid));
        child.kill().unwrap();
        child.wait().unwrap();
        let started = Instant::now();
        while alive(pid) {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the tool lives"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

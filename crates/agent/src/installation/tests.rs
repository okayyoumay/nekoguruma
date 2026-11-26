//! The installation file's case table (ADR-494): the rows are named in each test's comment. The
//! rows about the bytes and the keys run through [`parse`] in both builds; the rows about the
//! file system run through [`load`] with [`Policy::system`] where a refusal is the point, and, in
//! a debug build only, with the current user trusted as the owner of the test's own files
//! (`Policy::trusting_current_user` exists only there).

use std::io;
use std::sync::Mutex;

use shared_crypto::{Signer, SigningKey};
use tracing_subscriber::fmt::MakeWriter;

use super::*;

/// A value that no message may repeat.
const SECRET: &str = "SECRETVALUE";

fn root_pem() -> String {
    SigningKey::generate().unwrap().public_key().to_spki_pem()
}

/// A valid file: the three required keys.
fn minimal(mode: &str, server: &str) -> String {
    format!(
        "mode = \"{mode}\"\nserver_url = \"{server}\"\ndistribution_root_pem = \"\"\"\n{}\"\"\"\n",
        root_pem()
    )
}

fn device() -> String {
    minimal("device", "http://127.0.0.1:8080")
}

fn user() -> String {
    minimal("user", "http://127.0.0.1:8080")
}

fn ok(text: &str) -> Installation {
    parse(text.as_bytes()).unwrap_or_else(|error| panic!("{error}\n{text}"))
}

fn err(text: &str) -> InstallationError {
    parse(text.as_bytes()).expect_err("a file that must be refused was accepted")
}

/// Whether `error` refuses the value of `key` with `rule`, whether the value failed a rule or a
/// parser.
fn is_invalid(error: &InstallationError, key: &str, rule: &str) -> bool {
    matches!(
        error,
        InstallationError::Invalid { key: k, rule: r }
            | InstallationError::Unparsed { key: k, rule: r, .. }
            if *k == key && *r == rule
    )
}

/// An error that refuses `key`, for tests that need a value of the type.
fn invalid(key: &'static str, rule: &'static str) -> InstallationError {
    InstallationError::Invalid { key, rule }
}

/// A file with one more line.
fn with(text: &str, line: &str) -> String {
    format!("{text}{line}\n")
}

/// The file `text` with the line starting `key` replaced by `line`.
fn replacing(text: &str, key: &str, line: &str) -> String {
    let mut out = String::new();
    for l in text.lines() {
        if l.starts_with(key) {
            out.push_str(line);
        } else {
            out.push_str(l);
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The bytes: C4, C5, C6, D1, D9

/// D16, D10 to D12: a valid file gives its keys and the defaults.
#[test]
fn a_valid_file_gives_its_keys_and_the_defaults() {
    let device = ok(&device());
    assert_eq!(device.mode(), OperatingMode::Device);
    assert_eq!(device.server().as_str(), "http://127.0.0.1:8080");
    assert_eq!(device.profile(), Profile::Local);
    assert_eq!(device.startup(), Startup::Auto);
    assert_eq!(device.lock_dir(), None);
    let user = ok(&user());
    assert_eq!(user.mode(), OperatingMode::User);
    assert_eq!(user.startup(), Startup::Explicit);
    // The key is held parsed: its fingerprint is the one of the PEM in the file.
    let key = SigningKey::generate().unwrap().public_key();
    let text = format!(
        "mode = \"user\"\nserver_url = \"http://127.0.0.1\"\n\
         distribution_root_pem = {:?}\nprofile = \"local\"\nstartup = \"auto\"\n",
        key.to_spki_pem()
    );
    let installation = ok(&text);
    assert_eq!(
        installation.distribution_root().fingerprint_hex(),
        key.fingerprint_hex()
    );
    assert_eq!(installation.startup(), Startup::Auto);
}

/// C4: a file that is not UTF-8 is refused; a leading byte order mark is accepted.
#[test]
fn c4_a_file_that_is_not_utf8_is_refused_and_a_byte_order_mark_is_accepted() {
    let mut bytes = device().into_bytes();
    bytes.extend_from_slice(b"# \xff\xfe\n");
    assert!(matches!(parse(&bytes), Err(InstallationError::NotUtf8(_))));
    let mut with_mark = b"\xef\xbb\xbf".to_vec();
    with_mark.extend_from_slice(device().as_bytes());
    assert_eq!(parse(&with_mark).unwrap().mode(), OperatingMode::Device);
}

/// C5: at most 64 KiB; one byte more is refused before anything is parsed.
#[test]
fn c5_a_file_over_64_kib_is_refused() {
    let base = device();
    let pad = |size: usize| {
        let mut text = base.clone();
        // A comment line holds the padding, so the file stays valid.
        text.push_str("# ");
        let fill = size - text.len() - 1;
        text.push_str(&"x".repeat(fill));
        text.push('\n');
        assert_eq!(text.len(), size);
        text
    };
    parse(pad(MAX_SIZE as usize).as_bytes()).unwrap();
    assert!(matches!(
        parse(pad(MAX_SIZE as usize + 1).as_bytes()),
        Err(InstallationError::TooLarge)
    ));
    assert_eq!(InstallationError::TooLarge.exit_code(), 1);
}

/// C5: the read itself stops at the limit: a file of two bytes more is `TooLarge` without being
/// read whole.
#[cfg(unix)]
#[test]
fn c5_the_read_stops_at_the_limit() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("big.toml");
    std::fs::write(&path, vec![b'#'; MAX_SIZE as usize + 2]).unwrap();
    assert!(matches!(
        read_bytes(&path),
        Err(InstallationError::TooLarge)
    ));
    std::fs::write(&path, vec![b'#'; MAX_SIZE as usize]).unwrap();
    assert_eq!(read_bytes(&path).unwrap().len() as u64, MAX_SIZE);
}

/// C6: an empty file lacks the mode, the first required key.
#[test]
fn c6_an_empty_file_lacks_the_mode() {
    assert!(matches!(
        err(""),
        InstallationError::Missing { key: "mode" }
    ));
    assert!(matches!(
        err("# only a comment\n"),
        InstallationError::Missing { key: "mode" }
    ));
}

/// D1: a syntax error names its line and nothing of the file's text: `toml`'s own message quotes
/// the line, so it is never used.
#[test]
fn d1_a_syntax_error_names_the_line_only() {
    let text = format!("mode = \"device\"\nserver_url = {SECRET}\n");
    let error = err(&text);
    assert!(matches!(error, InstallationError::Syntax { line: Some(2) }));
    let shown = error.to_string();
    assert!(shown.contains("line 2"), "{shown}");
    assert!(
        !shown.contains(SECRET) && !shown.contains("server_url"),
        "{shown}"
    );
    // A repeated key is a syntax error as well.
    assert!(matches!(
        err("mode = \"user\"\nmode = \"user\"\n"),
        InstallationError::Syntax { line: Some(2) }
    ));
    // A line counted from the start, past a byte order mark.
    let mark = format!("\u{feff}# first\n{SECRET}\n");
    assert!(matches!(
        err(&mark),
        InstallationError::Syntax { line: Some(2) }
    ));
}

/// D9: CRLF line endings, in the file and inside the PEM, are accepted.
#[test]
fn d9_crlf_line_endings_are_accepted() {
    let text = device().replace('\n', "\r\n");
    assert!(text.contains("\r\n"));
    assert_eq!(ok(&text).mode(), OperatingMode::Device);
    // The PEM given as a one-line string with CRLF inside.
    let key = SigningKey::generate().unwrap().public_key();
    let pem = key.to_spki_pem().replace('\n', "\r\n");
    let text = format!(
        "mode = \"user\"\nserver_url = \"http://127.0.0.1:1\"\ndistribution_root_pem = {pem:?}\n"
    );
    assert_eq!(
        ok(&text).distribution_root().fingerprint_hex(),
        key.fingerprint_hex()
    );
}

// ---------------------------------------------------------------------------------------------
// The keys: D2 to D15

/// D2: an unknown key is refused, named only when it is a short word of lower-case letters,
/// digits, `_` and `-`; it is judged before the required keys.
#[test]
fn d2_an_unknown_key_is_refused_and_named_only_when_it_is_a_plain_word() {
    let named = err(&with(&device(), "moed = \"user\""));
    assert!(
        matches!(&named, InstallationError::UnknownKey { name: Some(name) } if name == "moed"),
        "{named}"
    );
    assert!(named.to_string().ends_with(": moed"), "{named}");
    for key in [
        "\"Bad Key\"".to_owned(),
        "\"with\\nnewline\"".to_owned(),
        "Upper".to_owned(),
        format!("\"{}\"", "k".repeat(MAX_KEY_NAME + 1)),
        "\"\"".to_owned(),
        // A table, or a dotted key, is named by its first part.
        format!("secret_table.{SECRET}"),
    ] {
        let error = err(&with(&device(), &format!("{key} = 1")));
        let InstallationError::UnknownKey { name } = &error else {
            panic!("{key}: {error}");
        };
        assert!(
            name.is_none() || name.as_deref() == Some("secret_table"),
            "{key}: {error}"
        );
        assert!(!error.to_string().contains(SECRET), "{error}");
    }
    // An unknown key is found even in a file that lacks every required key.
    assert!(matches!(
        err("typo = 1\n"),
        InstallationError::UnknownKey { name: Some(name) } if name == "typo"
    ));
    // A table named like a key is a key of the wrong type, not an unknown one.
    assert!(is_invalid(
        &err("[mode]\nx = 1\n"),
        "mode",
        "must be a string"
    ));
}

/// D3: each required key missing.
#[test]
fn d3_each_required_key_is_required() {
    let pem = format!("distribution_root_pem = {:?}\n", root_pem());
    let mode = "mode = \"device\"\n";
    let server = "server_url = \"http://127.0.0.1:1\"\n";
    for (text, key) in [
        (format!("{server}{pem}"), "mode"),
        (format!("{mode}{pem}"), "server_url"),
        (format!("{mode}{server}"), "distribution_root_pem"),
    ] {
        assert!(
            matches!(err(&text), InstallationError::Missing { key: got } if got == key),
            "{text}"
        );
    }
    // The optional keys do not stand in for them.
    assert!(matches!(
        err("profile = \"local\"\nstartup = \"auto\"\n"),
        InstallationError::Missing { key: "mode" }
    ));
    // The first missing key in the order of the table is the one named.
    assert!(matches!(
        err(mode),
        InstallationError::Missing { key: "server_url" }
    ));
}

/// D4: the mode is exactly `device` or `user`, as a string.
#[test]
fn d4_the_mode_is_device_or_user() {
    let rule = "must be \"device\" or \"user\"";
    for value in [
        "\"Device\"",
        "\"USER\"",
        "\"\"",
        "\" user\"",
        "\"machine\"",
        "\"user\\u0000\"",
    ] {
        let text = replacing(&device(), "mode", &format!("mode = {value}"));
        assert!(is_invalid(&err(&text), "mode", rule), "{value}");
    }
    for value in ["1", "true", "[\"device\"]", "{ a = 1 }"] {
        let text = replacing(&device(), "mode", &format!("mode = {value}"));
        assert!(
            is_invalid(&err(&text), "mode", "must be a string"),
            "{value}"
        );
    }
}

/// D5, D6: only the pinned origin is accepted, in the canonical form the record stores; no
/// refusal repeats the value.
#[test]
fn d5_d6_the_server_url_is_the_pinned_origin() {
    for good in [
        "http://127.0.0.1:8080/",
        "HTTP://127.0.0.1:8080",
        "http://127.0.0.1:8080",
    ] {
        let installation = ok(&minimal("user", good));
        assert_eq!(
            installation.server().as_str(),
            "http://127.0.0.1:8080",
            "{good}"
        );
    }
    // D6: the default port in either spelling is one origin.
    for good in [
        "HTTP://127.0.0.1:80/",
        "http://127.0.0.1",
        "http://127.0.0.1/",
    ] {
        let installation = ok(&minimal("user", good));
        assert_eq!(installation.server().as_str(), "http://127.0.0.1", "{good}");
        assert_eq!(installation.server().port(), 80);
    }
    for bad in [
        format!("https://{SECRET}.example"),
        format!("http://{SECRET}.example:8080"),
        "http://localhost:8080".to_owned(),
        "http://127.0.0.2:8080".to_owned(),
        "http://[::1]:8080".to_owned(),
        format!("http://127.0.0.1:8080/{SECRET}"),
        format!("http://{SECRET}@127.0.0.1:8080"),
        "http://127.0.0.1:99999".to_owned(),
        "127.0.0.1:8080".to_owned(),
        String::new(),
    ] {
        let error = err(&minimal("user", &bad));
        assert!(
            matches!(
                error,
                InstallationError::Unparsed {
                    key: "server_url",
                    ..
                }
            ),
            "{bad}: {error}"
        );
        assert!(
            std::error::Error::source(&error).is_some(),
            "{bad}: the parser's error is the source"
        );
        let shown = error.to_string();
        assert!(
            !shown.contains(SECRET) && !shown.contains("8080"),
            "{shown}"
        );
    }
    let text = replacing(&device(), "server_url", "server_url = 8080");
    assert!(is_invalid(&err(&text), "server_url", "must be a string"));
}

/// D7, D8: the key is one SPKI PEM block of a P-256 key; no refusal repeats a byte of it.
#[test]
fn d7_d8_the_distribution_root_is_one_p256_pem_block() {
    let good = root_pem();
    let rule = "must be one P-256 public key in SPKI PEM form";
    let with_pem = |pem: &str| {
        format!(
            "mode = \"user\"\nserver_url = \"http://127.0.0.1:1\"\ndistribution_root_pem = {pem:?}\n"
        )
    };
    let body: String = good.lines().filter(|l| !l.starts_with("-----")).collect();
    let mut cases = vec![
        // D7: not a PEM, no block, a wrong label, a truncated and a corrupted body.
        String::new(),
        format!("{SECRET}\n"),
        good.replace("PUBLIC KEY", "PRIVATE KEY"),
        good.replace("-----END PUBLIC KEY-----\n", ""),
        good.replace(&body[..8], &format!("{SECRET:.8}")),
        good.replacen("MFkw", "MFkx", 1),
        format!("text before\n{good}"),
        format!("{good}text after\n"),
        // D8: two blocks, the same or different keys.
        format!("{good}{good}"),
        format!("{good}{}", root_pem()),
    ];
    // A well-formed block with the right label whose body is not a P-256 key.
    cases.push(
        "-----BEGIN PUBLIC KEY-----\n\
         MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAEnl37bXdCLfDWNbvDTYn3LCm6gCNLeVyF\n\
         -----END PUBLIC KEY-----\n"
            .to_owned(),
    );
    for pem in cases {
        let error = err(&with_pem(&pem));
        assert!(is_invalid(&error, "distribution_root_pem", rule), "{pem}");
        let shown = error.to_string();
        assert!(
            !shown.contains(SECRET) && !shown.contains(&body[..16]),
            "{shown}"
        );
    }
    parse(with_pem(&good).as_bytes()).unwrap();
    assert!(is_invalid(
        &err("mode = \"user\"\nserver_url = \"http://127.0.0.1:1\"\ndistribution_root_pem = 1\n"),
        "distribution_root_pem",
        "must be a string"
    ));
}

/// D10: the profile is `local` by default; `standard` is refused until it exists.
#[test]
fn d10_the_profile_is_local_and_standard_is_refused() {
    assert_eq!(
        ok(&with(&device(), "profile = \"local\"")).profile(),
        Profile::Local
    );
    assert!(is_invalid(
        &err(&with(&device(), "profile = \"standard\"")),
        "profile",
        "standard is not supported yet"
    ));
    for bad in ["\"Local\"", "\"\"", "\"remote\"", "1"] {
        assert!(
            matches!(
                err(&with(&device(), &format!("profile = {bad}"))),
                InstallationError::Invalid { key: "profile", .. }
            ),
            "{bad}"
        );
    }
}

/// D11, D12: the startup defaults to `auto` for a device and `explicit` for a user; `explicit`
/// is refused for a device.
#[test]
fn d11_d12_the_startup_defaults_by_mode_and_explicit_is_refused_for_a_device() {
    for (text, expected) in [
        (with(&device(), "startup = \"auto\""), Startup::Auto),
        (with(&user(), "startup = \"explicit\""), Startup::Explicit),
        (with(&user(), "startup = \"auto\""), Startup::Auto),
    ] {
        assert_eq!(ok(&text).startup(), expected, "{text}");
    }
    assert!(is_invalid(
        &err(&with(&device(), "startup = \"explicit\"")),
        "startup",
        "must not be \"explicit\" in device mode"
    ));
    for text in [device(), user()] {
        for bad in ["\"manual\"", "\"\"", "\"Auto\"", "true"] {
            assert!(
                matches!(
                    err(&with(&text, &format!("startup = {bad}"))),
                    InstallationError::Invalid { key: "startup", .. }
                ),
                "{bad}"
            );
        }
    }
}

/// D13, D14: the lock directory is an absolute path, validated and kept; a relative, an empty
/// or an unusable one is refused.
#[test]
fn d13_d14_the_lock_dir_is_an_absolute_path() {
    let dir = std::env::temp_dir();
    let text = with(
        &device(),
        &format!("lock_dir = {:?}", dir.to_str().unwrap()),
    );
    assert_eq!(ok(&text).lock_dir(), Some(dir.as_path()));
    for bad in [
        "\"relative/locks\"",
        "\"\"",
        "\"locks\"",
        "\".\"",
        "\"/tmp\\u0000x\"",
        "1",
        "[\"/tmp\"]",
    ] {
        assert!(
            matches!(
                err(&with(&device(), &format!("lock_dir = {bad}"))),
                InstallationError::Invalid {
                    key: "lock_dir",
                    ..
                }
            ),
            "{bad}"
        );
    }
}

/// D15: the first problem in the fixed order decides: the syntax, an unknown key, then the keys
/// in the order of the table in the module's documentation.
#[test]
fn d15_the_first_problem_in_the_fixed_order_decides() {
    let keys = [
        "mode",
        "server_url",
        "distribution_root_pem",
        "profile",
        "startup",
        "lock_dir",
    ];
    let pem = format!("{:?}", root_pem());
    let good = [
        "\"device\"",
        "\"http://127.0.0.1:1\"",
        pem.as_str(),
        "\"local\"",
        "\"auto\"",
        "\"/x\"",
    ];
    for first in 0..keys.len() {
        // Every value from `first` on is a number, which none of the keys accepts.
        let text: String = (0..keys.len())
            .map(|i| format!("{} = {}\n", keys[i], if i < first { good[i] } else { "1" }))
            .collect();
        let error = err(&text);
        let InstallationError::Invalid { key, .. } = error else {
            panic!("{error}");
        };
        assert_eq!(key, keys[first], "{text}");
    }
    // A syntax error beats an unknown key, which beats a missing key.
    assert!(matches!(
        err("x = 1\n???\n"),
        InstallationError::Syntax { .. }
    ));
    assert!(matches!(
        err("x = 1\n"),
        InstallationError::UnknownKey { .. }
    ));
}

/// D16: a valid file is logged with the mode, the profile, the startup and the root key's
/// fingerprint; the path and the origin only in `external`; the PEM never.
#[test]
fn d16_the_log_names_the_decisions_and_keeps_the_origin_in_external() {
    #[derive(Clone, Default)]
    struct Log(std::sync::Arc<Mutex<Vec<u8>>>);
    impl io::Write for Log {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Log {
        type Writer = Log;
        fn make_writer(&'a self) -> Log {
            self.clone()
        }
    }
    let key = SigningKey::generate().unwrap().public_key();
    let pem = key.to_spki_pem();
    let text = format!(
        "mode = \"device\"\nserver_url = \"http://127.0.0.1:8080\"\ndistribution_root_pem = {pem:?}\n"
    );
    let installation = ok(&text);
    let log = Log::default();
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish(),
    );
    installation.log(Path::new("/some/dir/agent.toml"));
    drop(guard);
    let logged = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert!(logged.contains("installation file read"), "{logged}");
    assert!(logged.contains("mode=device"), "{logged}");
    assert!(
        logged.contains("profile=\"local\"") || logged.contains("profile=local"),
        "{logged}"
    );
    assert!(
        logged.contains("startup=\"auto\"") || logged.contains("startup=auto"),
        "{logged}"
    );
    assert!(logged.contains(&key.fingerprint_hex()), "{logged}");
    // The origin and the path are in `external`, and only there.
    let (before, external) = logged.split_once("external=").expect("an external field");
    assert!(external.contains("http://127.0.0.1:8080"), "{logged}");
    assert!(external.contains("/some/dir/agent.toml"), "{logged}");
    assert!(
        !before.contains("127.0.0.1") && !before.contains("agent.toml"),
        "{logged}"
    );
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    assert!(!logged.contains(&body[..16]), "{logged}");
}

/// Every error's exit status: 3 only for a read error other than a refusal of access (C1, C2).
#[test]
fn the_exit_status_is_3_only_for_another_read_error() {
    for (error, exit) in [
        (InstallationError::Absent, 1),
        (
            InstallationError::Refused {
                findings: String::new(),
            },
            1,
        ),
        // C1: the account may not read it: a restart would not help.
        (InstallationError::AccessDenied, 1),
        // C2: any other read error may clear.
        (InstallationError::Unreadable(io::ErrorKind::Other), 3),
        (InstallationError::Unreadable(io::ErrorKind::TimedOut), 3),
        (InstallationError::TooLarge, 1),
        (
            InstallationError::NotUtf8(String::from_utf8(vec![0xff]).unwrap_err().utf8_error()),
            1,
        ),
        (InstallationError::Syntax { line: None }, 1),
        (InstallationError::UnknownKey { name: None }, 1),
        (InstallationError::Missing { key: "mode" }, 1),
        (invalid("mode", "x"), 1),
    ] {
        assert_eq!(error.exit_code(), exit, "{error}");
    }
    // Neither the refusal's findings (which name paths) nor a value are in the text.
    let refused = InstallationError::Refused {
        findings: "/etc/x (naming file): bad".to_owned(),
    };
    assert!(!refused.to_string().contains("/etc"), "{refused}");
    assert!(
        refused
            .external(Path::new("/p/agent.toml"))
            .contains("/etc/x")
    );
    assert!(
        !InstallationError::Unreadable(io::ErrorKind::Other)
            .to_string()
            .contains('/')
    );
}

// ---------------------------------------------------------------------------------------------
// The file system: A2, B1 to B8, C1 to C3, F4

fn file_in(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("agent.toml");
    std::fs::write(&path, text).unwrap();
    path
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Whether the tests run as the super-user, for whom mode bits and ownership do not apply.
#[cfg(unix)]
#[expect(
    unsafe_code,
    reason = "libc FFI with no safe std equivalent: `geteuid` reads the effective user id (ADR-502)"
)]
fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(unix)]
fn make_writable_by_regular_users(path: &Path) {
    set_mode(path, 0o666);
}

#[cfg(windows)]
fn make_writable_by_regular_users(path: &Path) {
    // BUILTIN\Users gets write access to the file.
    let status = std::process::Command::new("icacls")
        .arg(path)
        .arg("/grant")
        .arg("*S-1-5-32-545:(W)")
        .status()
        .unwrap();
    assert!(status.success(), "icacls failed: {status}");
}

/// The owners the check trusts in the tests that make files of their own: the system's and the
/// user the tests run as.
#[cfg(debug_assertions)]
fn trusting() -> Policy {
    Policy::system().trusting_current_user()
}

/// A2: no file is absent, and a directory above it that does not exist makes it absent too.
#[test]
fn a2_no_file_is_absent() {
    let temp = tempfile::tempdir().unwrap();
    for path in [
        temp.path().join("agent.toml"),
        temp.path().join("missing").join("agent.toml"),
    ] {
        let error = load(&path, &Policy::system()).unwrap_err();
        assert!(matches!(error, InstallationError::Absent));
        assert_eq!(error.exit_code(), 1);
    }
}

/// B1, W: a file that regular users can change is refused whatever its content says, the user
/// mode's included: no mode makes the check lenient.
#[test]
fn b1_a_file_regular_users_can_change_is_refused_in_both_modes() {
    for text in [device(), user()] {
        let temp = tempfile::tempdir().unwrap();
        let path = file_in(temp.path(), &text);
        make_writable_by_regular_users(&path);
        // Debug builds trust the test's own user, so the refusal is for the mode bits alone.
        #[cfg(debug_assertions)]
        let policy = trusting();
        #[cfg(not(debug_assertions))]
        let policy = Policy::system();
        let error = load(&path, &policy).unwrap_err();
        let InstallationError::Refused { findings } = &error else {
            panic!("{error}");
        };
        assert!(findings.contains(path.to_str().unwrap()), "{findings}");
        #[cfg(debug_assertions)]
        assert!(findings.contains("writable by regular users"), "{findings}");
        assert_eq!(error.exit_code(), 1);
    }
}

/// A valid file the check accepts, read whole.
#[cfg(debug_assertions)]
#[test]
fn a_file_the_check_accepts_is_read() {
    let temp = tempfile::tempdir().unwrap();
    let path = file_in(temp.path(), &user());
    let installation = load(&path, &trusting()).unwrap();
    assert_eq!(installation.mode(), OperatingMode::User);
    // F4: the same file under the system owners only is refused when its owner is not one of
    // them (a user's own file); as the super-user the owner is a system one.
    #[cfg(unix)]
    if !is_root() {
        assert!(matches!(
            load(&path, &Policy::system()),
            Err(InstallationError::Refused { .. })
        ));
    }
}

/// B2: a file owned by a user the policy does not trust is refused.
#[cfg(unix)]
#[test]
fn b2_a_file_owned_by_an_untrusted_user_is_refused() {
    if is_root() {
        eprintln!("skipped: the super-user owns every file the test makes");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let path = file_in(temp.path(), &device());
    let Err(InstallationError::Refused { findings }) = load(&path, &Policy::system()) else {
        panic!("a file of the test's user was not refused");
    };
    assert!(findings.contains("not trusted"), "{findings}");
}

/// B3, B4: a directory above the file that others can write is refused, unless it is sticky.
#[cfg(all(unix, debug_assertions))]
#[test]
fn b3_b4_a_writable_ancestor_is_refused_unless_it_is_sticky() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("shared");
    std::fs::create_dir(&dir).unwrap();
    let path = file_in(&dir, &device());
    set_mode(&dir, 0o777);
    assert!(matches!(
        load(&path, &trusting()),
        Err(InstallationError::Refused { .. })
    ));
    // Group-writable is writable too.
    set_mode(&dir, 0o775);
    assert!(matches!(
        load(&path, &trusting()),
        Err(InstallationError::Refused { .. })
    ));
    // B4: sticky, as /tmp is: others may add entries but not replace the file.
    set_mode(&dir, 0o1777);
    load(&path, &trusting()).unwrap();
    // A directory that only its owner can write is accepted.
    set_mode(&dir, 0o755);
    load(&path, &trusting()).unwrap();
}

/// B5, B6: a link is followed when every entry on the way passes (a trusted owner's link is
/// accepted, ADR-494), and refused when its target is in a directory others can write.
#[cfg(all(unix, debug_assertions))]
#[test]
fn b5_b6_a_link_is_judged_with_its_target() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let target = file_in(&real, &device());
    let link = temp.path().join("link.toml");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    load(&link, &trusting()).unwrap();
    // A target in a directory others can write is refused through the link.
    set_mode(&real, 0o777);
    assert!(matches!(
        load(&link, &trusting()),
        Err(InstallationError::Refused { .. })
    ));
    // A link to nothing is absent only when the link itself is missing: a dangling one is
    // refused or unreadable, never accepted.
    set_mode(&real, 0o755);
    std::fs::remove_file(&target).unwrap();
    load(&link, &trusting()).unwrap_err();
}

/// B7: the path is not a regular file (a directory) and is refused.
#[cfg(debug_assertions)]
#[test]
fn b7_a_directory_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent.toml");
    std::fs::create_dir(&path).unwrap();
    let error = load(&path, &trusting()).unwrap_err();
    assert!(
        matches!(error, InstallationError::Refused { .. }),
        "{error}"
    );
    assert_eq!(error.exit_code(), 1);
}

/// B8: an entry the check cannot inspect is a refusal, not a read error.
#[cfg(unix)]
#[test]
fn b8_an_uninspectable_entry_is_refused() {
    if is_root() {
        eprintln!("skipped: the super-user can inspect every entry");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("closed");
    std::fs::create_dir(&dir).unwrap();
    let path = file_in(&dir, &device());
    set_mode(&dir, 0o000);
    let result = load(&path, &Policy::system());
    set_mode(&dir, 0o700);
    let error = result.unwrap_err();
    assert!(
        matches!(error, InstallationError::Refused { .. }),
        "{error}"
    );
    assert_eq!(error.exit_code(), 1);
}

/// C1, Q1: a file that passed the check but that the account may not open is exit 1: no restart
/// gives the account access.
#[cfg(all(unix, debug_assertions))]
#[test]
fn c1_a_file_the_account_cannot_open_is_exit_1() {
    if is_root() {
        eprintln!("skipped: the super-user opens every file");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let path = file_in(temp.path(), &device());
    set_mode(&path, 0o000);
    let error = load(&path, &trusting()).unwrap_err();
    assert!(matches!(error, InstallationError::AccessDenied));
    assert_eq!(error.exit_code(), 1);
}

/// C2: any other read error is exit 3. A directory opens on Unix and fails to read.
#[cfg(unix)]
#[test]
fn c2_another_read_error_is_exit_3() {
    let temp = tempfile::tempdir().unwrap();
    let error = read_bytes(temp.path()).unwrap_err();
    assert!(matches!(error, InstallationError::Unreadable(_)), "{error}");
    assert_eq!(error.exit_code(), 3);
}

/// C3: a file removed between the check and the open is absent (exit 1), not a read error.
#[test]
fn c3_a_file_removed_after_the_check_is_absent() {
    let temp = tempfile::tempdir().unwrap();
    let error = read_bytes(&temp.path().join("agent.toml")).unwrap_err();
    assert!(matches!(error, InstallationError::Absent));
    assert_eq!(error.exit_code(), 1);
}

/// A file read from disk is judged by the same rules as its bytes.
#[cfg(debug_assertions)]
#[test]
fn a_file_with_a_bad_key_is_refused_after_the_check() {
    let temp = tempfile::tempdir().unwrap();
    let path = file_in(
        temp.path(),
        &replacing(&device(), "mode", "mode = \"machine\""),
    );
    assert!(
        matches!(&load(&path, &trusting()), Err(e) if is_invalid(e, "mode", "must be \"device\" or \"user\""))
    );
}

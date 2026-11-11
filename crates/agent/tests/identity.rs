//! `ngr-agent identity` (design 6.4, ADR-459, ADR-460): the key created once, found again by the
//! next process, and the exit codes. Every process gets its own `HOME`, `XDG_STATE_HOME` and
//! `LOCALAPPDATA`, so none touches the account's real state.
//!
//! A release build has no plain-file store, so the tests that need one run in debug builds only;
//! the ones that check what a release build must not do run in both.

use std::path::Path;
use std::process::{Command, Output};

#[cfg(debug_assertions)]
use std::process::{Child, Stdio};

#[cfg(debug_assertions)]
use shared_crypto::PublicKey;

const KEY_STORE_VAR: &str = "NGR_AGENT_KEY_STORE";
const STATE_DIR_VAR: &str = "NGR_AGENT_STATE_DIR";

/// `ngr-agent` with the account's state redirected into `home` and neither debug variable set.
fn ngr_agent(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ngr-agent"));
    command
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("xdg"))
        .env("LOCALAPPDATA", home.join("local"))
        .env_remove(KEY_STORE_VAR)
        .env_remove(STATE_DIR_VAR);
    command
}

/// The identity command with the plain-file store in `state`.
#[cfg(debug_assertions)]
fn identity(home: &Path, state: &Path, mode: &str) -> Command {
    let mut command = ngr_agent(home);
    command
        .args(["identity", "--mode", mode])
        .env(KEY_STORE_VAR, "file")
        .env(STATE_DIR_VAR, state);
    command
}

/// What `identity` printed: the PEM, the fingerprint and whether this process created the key.
#[cfg(debug_assertions)]
struct Printed {
    pem: String,
    fingerprint: String,
    created: bool,
}

#[cfg(debug_assertions)]
fn printed(output: &Output) -> Printed {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    assert_eq!(text.lines().count(), 1, "one JSON line: {text:?}");
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let object = json.as_object().unwrap();
    assert_eq!(object.len(), 3, "{json}");
    let pem = json["public_key_pem"].as_str().unwrap().to_owned();
    let fingerprint = json["fingerprint"].as_str().unwrap().to_owned();
    // The PEM is a public key document and the fingerprint is the one of that key.
    let key = PublicKey::from_spki_pem(&pem).expect("the printed PEM is an SPKI document");
    assert_eq!(key.fingerprint_hex(), fingerprint);
    assert_eq!(fingerprint.len(), 64);
    Printed {
        pem,
        fingerprint,
        created: json["created"].as_bool().unwrap(),
    }
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

fn assert_exit(output: &Output, code: i32) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(code), "{stderr}");
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
}

/// E5: only `--mode` is accepted, and it is required.
#[test]
fn a_bad_command_line_exits_with_2() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        &["identity"][..],
        &["identity", "--mode", "machine"],
        &["identity", "--mode", "user", "--vci", "x"],
        &["identity", "--vci", "x"],
        &["identity", "--mode"],
    ] {
        let output = ngr_agent(home.path()).args(args).output().unwrap();
        assert_exit(&output, 2);
        assert!(String::from_utf8_lossy(&output.stderr).contains("usage: ngr-agent"));
    }
}

/// S8: with no debug variables no store is selected, in either build, and nothing is written.
#[test]
fn without_a_store_the_exit_is_1_and_nothing_is_written() {
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    for mode in ["device", "user"] {
        let output = ngr_agent(home.path())
            .args(["identity", "--mode", mode])
            .env(STATE_DIR_VAR, state.path())
            .output()
            .unwrap();
        assert_exit(&output, 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("no key store"));
    }
    assert!(is_empty_dir(state.path()));
}

/// E2: a release build reads neither variable and writes no plain key file; a debug build does
/// not either when a variable is missing or wrong (E3). The invariant is the same: the override
/// directory stays empty.
#[test]
fn a_missing_or_wrong_variable_selects_no_store_and_writes_no_key() {
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let relative = "relative-state";
    let cases: [Vec<(&str, &Path)>; 3] = [
        vec![(KEY_STORE_VAR, Path::new("file"))],
        vec![
            (KEY_STORE_VAR, Path::new("plain")),
            (STATE_DIR_VAR, state.path()),
        ],
        vec![
            (KEY_STORE_VAR, Path::new("file")),
            (STATE_DIR_VAR, Path::new(relative)),
        ],
    ];
    for envs in cases {
        let mut command = ngr_agent(home.path());
        command.args(["identity", "--mode", "user"]);
        for (name, value) in &envs {
            command.env(name, value);
        }
        assert_exit(&command.output().unwrap(), 1);
    }
    assert!(is_empty_dir(state.path()));
    assert!(!Path::new(relative).exists());
}

/// A release build ignores both variables even when they are right (E2).
#[cfg(not(debug_assertions))]
#[test]
fn a_release_build_has_no_file_store() {
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    for mode in ["device", "user"] {
        let output = ngr_agent(home.path())
            .args(["identity", "--mode", mode])
            .env(KEY_STORE_VAR, "file")
            .env(STATE_DIR_VAR, state.path())
            .output()
            .unwrap();
        assert_exit(&output, 1);
    }
    assert!(is_empty_dir(state.path()));
}

/// A variant's document whose procedure is `SecurityAccess`, which every build refuses (ADR-259),
/// so `run` gets past reading the document and checking the job, as far as it goes before a
/// worker starts.
fn refused_document(dir: &Path) -> std::path::PathBuf {
    use diag_ir::declaration::{EcuDocumentT, EcuVariantT, ProtocolT, encode_ecu_document};
    use diag_ir::{IR_SCHEMA_VERSION, Op, Program};
    let program = Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![Op::PushBytes(0), Op::SecurityAccess { level: 1 }],
        constants: vec![vec![0xF1, 0x90]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
        targets: Vec::new(),
    };
    let mut protocol = ProtocolT::default();
    protocol.short_name = Some("ISO15765".into());
    protocol.application_layer = Some("UDS".into());
    protocol.baud_rate = 500_000;
    protocol.tx_id = 0x7E0;
    protocol.rx_id = 0x7E8;
    let mut variant = EcuVariantT::default();
    variant.id = 1;
    variant.short_name = Some("v".into());
    variant.protocols = Some(vec![protocol]);
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("csv-starlark".into());
    document.source_digest = Some("00".into());
    document.variant = Some(Box::new(variant));
    document.procedure = Some(postcard::to_allocvec(&program).unwrap());
    let path = dir.join("variant.dir");
    std::fs::write(&path, encode_ecu_document(&document)).unwrap();
    path
}

/// E1: `run` and `identify` do not touch the key store or the state directory, with both
/// variables set. Neither is driven to a worker here (that needs a VCI, which the end-to-end
/// tests of the worker packages have): each goes as far as the agent's own checks refuse the job,
/// after argument parsing and reading its inputs, which is where a store access added to the
/// command would sit.
#[test]
fn run_and_identify_leave_the_state_directory_alone() {
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let inputs = tempfile::tempdir().unwrap();
    let document = refused_document(inputs.path());
    let garbage_plan = inputs.path().join("plan.bin");
    std::fs::write(&garbage_plan, b"not a plan").unwrap();
    let run = ngr_agent(home.path())
        .args(["run", "--vci", "none", "--mode", "user", "--document"])
        .arg(&document)
        .env(KEY_STORE_VAR, "file")
        .env(STATE_DIR_VAR, state.path())
        .output()
        .unwrap();
    assert_exit(&run, 1);
    assert!(String::from_utf8_lossy(&run.stderr).contains("refused"));
    let identify = ngr_agent(home.path())
        .args(["identify", "--vci", "none", "--mode", "user", "--plan"])
        .arg(&garbage_plan)
        .env(KEY_STORE_VAR, "file")
        .env(STATE_DIR_VAR, state.path())
        .output()
        .unwrap();
    assert_exit(&identify, 1);
    assert!(String::from_utf8_lossy(&identify.stderr).contains("plan refused"));
    assert!(is_empty_dir(state.path()));
}

/// S1, S2: the first process creates the key; the next finds the same one after the first has
/// exited; the modes keep separate keys.
#[cfg(debug_assertions)]
#[test]
fn a_second_process_finds_the_key_the_first_created() {
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let first = printed(
        &identity(home.path(), state.path(), "user")
            .output()
            .unwrap(),
    );
    assert!(first.created);
    let second = printed(
        &identity(home.path(), state.path(), "user")
            .output()
            .unwrap(),
    );
    assert!(!second.created);
    assert_eq!(second.pem, first.pem);
    assert_eq!(second.fingerprint, first.fingerprint);
    // The device mode has a name of its own.
    let device = printed(
        &identity(home.path(), state.path(), "device")
            .output()
            .unwrap(),
    );
    assert!(device.created);
    assert_ne!(device.pem, first.pem);
    // One key file and one lock file per mode, and no temporary file left.
    let mut names: Vec<String> = std::fs::read_dir(state.path().join("identity"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "nekoguruma-agent-key.device.v1.lock",
            "nekoguruma-agent-key.device.v1.pk8",
            "nekoguruma-agent-key.user.v1.lock",
            "nekoguruma-agent-key.user.v1.pk8",
        ]
    );
}

/// L1: processes racing from an empty store all print the same key, and exactly one of them
/// created it.
#[cfg(debug_assertions)]
#[test]
fn processes_racing_to_create_make_one_key() {
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let children: Vec<Child> = (0..8)
        .map(|_| {
            identity(home.path(), state.path(), "user")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let all: Vec<Printed> = children
        .into_iter()
        .map(|child| printed(&child.wait_with_output().unwrap()))
        .collect();
    assert!(all.iter().all(|one| one.pem == all[0].pem));
    assert_eq!(all.iter().filter(|one| one.created).count(), 1);
    let keys = std::fs::read_dir(state.path().join("identity"))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".pk8")
        })
        .count();
    assert_eq!(keys, 1);
}

/// S4/S5, exit 4: a key file that does not decode is unusable, and is left as it is.
#[cfg(debug_assertions)]
#[test]
fn an_unusable_key_exits_with_4_and_is_not_replaced() {
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let dir = state.path().join("identity");
    std::fs::create_dir(&dir).unwrap();
    let file = dir.join("nekoguruma-agent-key.user.v1.pk8");
    std::fs::write(&file, b"not a key").unwrap();
    let output = identity(home.path(), state.path(), "user")
        .output()
        .unwrap();
    assert_exit(&output, 4);
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used"));
    assert_eq!(std::fs::read(&file).unwrap(), b"not a key");
}

/// L7, exit 3: with the key absent and the lock unopenable (a directory stands at the lock's
/// name, which fails for root as well), the exit is 3 and no key is created.
#[cfg(debug_assertions)]
#[test]
fn a_lock_that_cannot_be_opened_exits_with_3() {
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let lock = state
        .path()
        .join("identity")
        .join("nekoguruma-agent-key.user.v1.lock");
    std::fs::create_dir_all(&lock).unwrap();
    let output = identity(home.path(), state.path(), "user")
        .output()
        .unwrap();
    assert_exit(&output, 3);
    assert!(
        !state
            .path()
            .join("identity")
            .join("nekoguruma-agent-key.user.v1.pk8")
            .exists()
    );
}

/// L9, exit 3: an `identity` directory that others may write is refused (mode bits, which hold
/// for root too), and no key is created in it.
#[cfg(all(debug_assertions, unix))]
#[test]
fn a_world_writable_identity_directory_exits_with_3() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let dir = state.path().join("identity");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let output = identity(home.path(), state.path(), "user")
        .output()
        .unwrap();
    assert_exit(&output, 3);
    assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
}

/// A regular file where the `identity` directory belongs: the store cannot even be read, so the
/// exit is 3 (S3) and the file is as it was.
#[cfg(debug_assertions)]
#[test]
fn an_identity_path_that_is_a_file_exits_with_3() {
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(state.path().join("identity"), b"a file").unwrap();
    let output = identity(home.path(), state.path(), "user")
        .output()
        .unwrap();
    assert_exit(&output, 3);
    assert_eq!(
        std::fs::read(state.path().join("identity")).unwrap(),
        b"a file"
    );
}

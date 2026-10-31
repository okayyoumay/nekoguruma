//! `ngr-agent` failures that end the run before a worker starts. A run against a worker is
//! tested end to end in `crates/j2534-0404-service/tests/ngr_agent_end_to_end.rs`, which also
//! relies on this file: cargo builds the `ngr-agent` binary for a package's integration tests.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use diag_ir::{IR_SCHEMA_VERSION, Op, Program};

fn ngr_agent() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ngr-agent"))
}

/// A fresh temporary directory, removed however the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("ngr-agent-cli-{name}-{pid}-{nanos}"));
        std::fs::create_dir_all(&path).expect("temporary directory should be writable");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The program a document's procedure is: `code` over one constant, the VIN's DID.
fn program(code: Vec<Op>) -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code,
        constants: vec![vec![0xF1, 0x90]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    }
}

/// A variant's document whose procedure is `code`.
fn write_program(dir: &Path, code: Vec<Op>) -> PathBuf {
    let procedure = postcard::to_allocvec(&program(code)).expect("program should serialize");
    write_document(dir, Some(procedure))
}

// Only the debug-only tests read the VIN through a worker.
#[cfg(debug_assertions)]
fn read_vin(dir: &Path) -> PathBuf {
    write_program(
        dir,
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }],
    )
}

/// Asserts a failure with exit status 1, nothing on stdout and `message` on stderr.
fn assert_fails_with(output: Output, message: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains(message), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn usage_error_exits_with_2() {
    let output = ngr_agent().output().expect("ngr-agent should run");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage: ngr-agent run"));
    assert!(output.stdout.is_empty());
}

#[test]
fn the_retired_program_and_link_options_are_usage_errors() {
    for extra in [
        &["--program", "p.json"][..],
        &["--document", "d.dir", "--tx-id", "7E0"],
        &["--document", "d.dir", "--rx-id", "7E8"],
    ] {
        let output = ngr_agent()
            .args(["run", "--vci", "x", "--mode", "user"])
            .args(extra)
            .output()
            .expect("ngr-agent should run");
        assert_eq!(output.status.code(), Some(2), "{extra:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("unknown option"), "{stderr}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn a_run_without_a_document_is_a_usage_error() {
    let output = ngr_agent()
        .args(["run", "--vci", "x", "--mode", "user"])
        .output()
        .expect("ngr-agent should run");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--document is required"));
    assert!(output.stdout.is_empty());
}

#[test]
fn refused_program_exits_with_1_before_resolving_the_vci() {
    let temp = TempDir::new("refused");
    // SecurityAccess is refused in every build: a standalone agent has no key source (ADR-259).
    let program = write_program(
        &temp.0,
        vec![Op::PushBytes(0), Op::SecurityAccess { level: 1 }],
    );
    let output = ngr_agent()
        .args(["run", "--vci", "none", "--mode", "user", "--document"])
        .arg(program)
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "refused");
}

/// A variant's document with no services, carrying `procedure`. Its protocol is UDS over
/// ISO 15765 at 500 kbit/s on 7E0 / 7E8, so a job that gets as far as the link can open one.
fn write_document(dir: &Path, procedure: Option<Vec<u8>>) -> PathBuf {
    use diag_ir::declaration::{EcuDocumentT, EcuVariantT, ProtocolT, encode_ecu_document};
    let mut variant = EcuVariantT::default();
    variant.id = 1;
    variant.short_name = Some("v".into());
    let mut protocol = ProtocolT::default();
    protocol.short_name = Some("ISO15765".into());
    protocol.application_layer = Some("UDS".into());
    protocol.baud_rate = 500_000;
    protocol.tx_id = 0x7E0;
    protocol.rx_id = 0x7E8;
    variant.protocols = Some(vec![protocol]);
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("csv-starlark".into());
    document.source_digest = Some("00".into());
    document.variant = Some(Box::new(variant));
    document.procedure = procedure;
    let path = dir.join("variant.dir");
    std::fs::write(&path, encode_ecu_document(&document)).expect("document should be writable");
    path
}

#[test]
fn a_document_that_cannot_run_exits_with_1_before_resolving_the_vci() {
    let temp = TempDir::new("document");
    let not_a_document = temp.0.join("not.dir");
    std::fs::write(&not_a_document, b"not a document").expect("file should be writable");
    // A field the document does not declare.
    let decodes = Program {
        constants: vec![vec![0x62]],
        ..program(vec![
            Op::PushBytes(0),
            Op::DecodeField {
                service: 1,
                field: 1,
            },
        ])
    };
    let undeclared = write_document(
        &temp.0,
        Some(postcard::to_allocvec(&decodes).expect("program should serialize")),
    );
    let cases = [
        (temp.0.join("missing.dir"), "cannot read"),
        (not_a_document, "not a variant document"),
        (undeclared, "does not declare"),
    ];
    for (path, message) in cases {
        let output = ngr_agent()
            .args(["run", "--vci", "none", "--mode", "user", "--document"])
            .arg(&path)
            .output()
            .expect("ngr-agent should run");
        assert_fails_with(output, message);
    }
    let without_procedure = TempDir::new("no-procedure");
    let output = ngr_agent()
        .args(["run", "--vci", "none", "--mode", "user", "--document"])
        .arg(write_document(&without_procedure.0, None))
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "carries no procedure");
}

/// The flash artifact is read and compared with the hash the command line names before a worker
/// starts (ADR-336 item 1); the options come together.
#[test]
fn a_bad_artifact_exits_before_resolving_the_vci() {
    let temp = TempDir::new("artifact");
    let document = write_program(&temp.0, vec![Op::PushBytes(0), Op::Pop]);
    let artifact = temp.0.join("flash.bin");
    std::fs::write(&artifact, b"abc").expect("artifact should be writable");
    let run = |artifact: &Path, sha256: &str| {
        ngr_agent()
            .args(["run", "--vci", "none", "--mode", "user", "--document"])
            .arg(&document)
            .arg("--artifact")
            .arg(artifact)
            .args(["--artifact-sha256", sha256])
            .output()
            .expect("ngr-agent should run")
    };
    // The SHA-256 of "abc" with its first digit changed.
    let wrong = "aa7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_fails_with(run(&artifact, wrong), "the job names");
    assert_fails_with(run(&temp.0.join("missing.bin"), wrong), "cannot read");
    // Only one of the two options is a usage error.
    let output = ngr_agent()
        .args(["run", "--vci", "none", "--mode", "user", "--document"])
        .arg(&document)
        .arg("--artifact")
        .arg(&artifact)
        .output()
        .expect("ngr-agent should run");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be given together"));
}

/// A release build refuses every write before a worker starts (ADR-235 item 8, ADR-247); a
/// debug build allows them on the simulator, so it cannot refuse them up front.
#[cfg(not(debug_assertions))]
#[test]
fn a_write_is_refused_before_resolving_the_vci_in_a_release_build() {
    let temp = TempDir::new("refused-write");
    // WriteDataByIdentifier is outside the read-only policy.
    let program = write_program(
        &temp.0,
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x2E }],
    );
    let output = ngr_agent()
        .args(["run", "--vci", "none", "--mode", "user", "--document"])
        .arg(program)
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "refused");
}

/// A release build refuses TesterPresent in a procedure, as it refuses every request outside the
/// read-only set, before a worker starts. Only the identification job's presence request sends
/// it (ADR-396); a debug build allows any request on the simulator (ADR-247), so it cannot
/// refuse it up front.
#[cfg(not(debug_assertions))]
#[test]
fn tester_present_is_refused_before_resolving_the_vci_in_a_release_build() {
    let temp = TempDir::new("refused-tester-present");
    let program = write_program(
        &temp.0,
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x3E }],
    );
    let output = ngr_agent()
        .args(["run", "--vci", "none", "--mode", "user", "--document"])
        .arg(program)
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "refused");
}

/// `ngr-agent identify`'s usage errors exit with 2 and print nothing on stdout (design 8.8.2).
#[test]
fn identify_usage_errors_exit_with_2() {
    let base = [
        "identify", "--vci", "x", "--mode", "user", "--plan", "p.bin",
    ];
    for args in [
        // The plan, the VCI and the mode are required.
        &["identify", "--vci", "x", "--mode", "user"][..],
        &["identify", "--mode", "user", "--plan", "p.bin"],
        &["identify", "--vci", "x", "--plan", "p.bin"],
    ] {
        let output = ngr_agent()
            .args(args)
            .output()
            .expect("ngr-agent should run");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
    }
    for extra in [
        // One plan; the artifact options are a run's; an unknown option.
        &["--plan", "q.bin"][..],
        &["--artifact", "a.bin"],
        &["--artifact-sha256", "00"],
        &["--bogus", "1"],
    ] {
        let output = ngr_agent()
            .args(base)
            .args(extra)
            .output()
            .expect("ngr-agent should run");
        assert_eq!(output.status.code(), Some(2), "{extra:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("usage: ngr-agent"), "{stderr}");
        assert!(output.stdout.is_empty());
    }
}

/// A plan or a document that cannot be read or is refused ends the run with 1 before the VCI:
/// no log directory, no lock directory and no worker (design 8.8.2). The VCI is not resolvable
/// here, so reaching it would fail with another message.
#[test]
fn identify_refuses_a_bad_plan_before_the_vci() {
    use diag_ir::plan::{IdentificationPlan, PLAN_SCHEMA_VERSION, PlanDefinition, Selection};
    let temp = TempDir::new("identify-refused");
    let logs = temp.0.join("logs");
    let locks = temp.0.join("locks");
    let identify = |plan: &Path, documents: &[&Path]| {
        let mut agent = ngr_agent();
        agent
            .args(["identify", "--vci", "none", "--mode", "user", "--plan"])
            .arg(plan)
            .arg("--comm-log")
            .arg(&logs)
            .arg("--locks")
            .arg(&locks);
        for document in documents {
            agent.arg("--document").arg(document);
        }
        agent.output().expect("ngr-agent should run")
    };

    // A plan that is not there.
    assert_fails_with(identify(&temp.0.join("missing.bin"), &[]), "cannot read");

    // A file that is not a plan.
    let garbage = temp.0.join("garbage.bin");
    std::fs::write(&garbage, b"not a plan").expect("file should be writable");
    assert_fails_with(identify(&garbage, &[]), "plan refused");

    // A plan that names a document nobody supplied.
    let plan = IdentificationPlan {
        schema_version: PLAN_SCHEMA_VERSION,
        selection: Selection::EveryEcu,
        candidates: Vec::new(),
        definitions: vec![PlanDefinition {
            id: "ecu".to_owned(),
            documents: vec![[7; 32]],
        }],
    };
    let named = temp.0.join("plan.bin");
    std::fs::write(&named, plan.encode().expect("the plan encodes")).expect("plan file");
    assert_fails_with(identify(&named, &[]), "was not supplied");

    // A document that is not there.
    assert_fails_with(
        identify(&named, &[&temp.0.join("missing.dir")]),
        "cannot read",
    );

    assert!(!logs.exists(), "a refused plan creates no log directory");
    assert!(!locks.exists(), "a refused plan takes no lock");
}

// The tests below point the agent at a test registration-definition directory through the
// debug-only NGR_J2534_DEFINITION_DIR override (ADR-282), which replaces the Linux definition
// directory and the Windows registry alike.

/// A definition directory that names `library` as the VCI `test-vci`, or names nothing.
#[cfg(debug_assertions)]
fn write_definitions(dir: &Path, library: Option<&Path>) -> PathBuf {
    let definitions = dir.join("definitions");
    std::fs::create_dir_all(&definitions).expect("definition directory should be writable");
    if let Some(library) = library {
        let library = std::path::absolute(library).expect("library path should be absolute");
        let text = format!(
            "Name = \"test-vci\"\nFunctionLibrary = {:?}\n",
            library.display().to_string()
        );
        std::fs::write(definitions.join("test-vci.toml"), text)
            .expect("definition file should be writable");
    }
    definitions
}

#[cfg(debug_assertions)]
#[test]
fn unknown_vci_exits_with_1() {
    let temp = TempDir::new("unknown-vci");
    let definitions = write_definitions(&temp.0, None);
    let output = ngr_agent()
        .args(["run", "--vci", "test-vci", "--mode", "user", "--document"])
        .arg(read_vin(&temp.0))
        .arg("--workers")
        .arg(&temp.0)
        .arg("--locks")
        .arg(temp.0.join("locks"))
        .env("NGR_J2534_DEFINITION_DIR", &definitions)
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "cannot resolve the library of VCI \"test-vci\"");
}

#[cfg(debug_assertions)]
#[test]
fn missing_worker_build_exits_with_1() {
    let temp = TempDir::new("no-worker");
    // Any library header does: the agent's own executable stands in for the VCI library.
    let definitions = write_definitions(&temp.0, Some(Path::new(env!("CARGO_BIN_EXE_ngr-agent"))));
    let output = ngr_agent()
        .args(["run", "--vci", "test-vci", "--mode", "user", "--document"])
        .arg(read_vin(&temp.0))
        .arg("--workers")
        .arg(temp.0.join("empty"))
        .arg("--locks")
        .arg(temp.0.join("locks"))
        .env("NGR_J2534_DEFINITION_DIR", &definitions)
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "no worker bundled for ABI");
}

/// Installs, for the agent's own ABI, a stand-in worker that records the `NGR_J2534_LONG_SIZE`
/// it was started with in `<workers>/long_size` and exits, and runs the agent against a VCI whose
/// definition carries `extra` (the agent's executable stands in for the library). Returns what
/// the stand-in recorded.
#[cfg(all(debug_assertions, unix))]
fn long_size_passed_to_the_worker(name: &str, extra: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new(name);
    let agent = Path::new(env!("CARGO_BIN_EXE_ngr-agent"));
    let definitions = write_definitions(&temp.0, Some(agent));
    let file = definitions.join("test-vci.toml");
    let text = std::fs::read_to_string(&file).expect("definition file should be readable");
    std::fs::write(&file, format!("{text}{extra}")).expect("definition file should be writable");
    let abi = worker_host::abi::detect_file(agent).expect("the agent's header should be readable");
    let workers = temp.0.join("workers");
    let dir = workers.join(abi.name());
    std::fs::create_dir_all(&dir).expect("workers directory should be writable");
    let record = workers.join("long_size");
    let worker = dir.join("j2534-0404-service");
    std::fs::write(
        &worker,
        format!(
            "#!/bin/sh\nprintf '%s' \"${{NGR_J2534_LONG_SIZE-unset}}\" > '{}'\nexit 1\n",
            record.display()
        ),
    )
    .expect("stand-in worker should be writable");
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755))
        .expect("stand-in worker should be executable");
    let output = ngr_agent()
        .args(["run", "--vci", "test-vci", "--mode", "user", "--document"])
        .arg(read_vin(&temp.0))
        .arg("--workers")
        .arg(&workers)
        .arg("--locks")
        .arg(temp.0.join("locks"))
        .env("NGR_J2534_DEFINITION_DIR", &definitions)
        .output()
        .expect("ngr-agent should run");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    std::fs::read_to_string(&record)
        .unwrap_or_else(|error| panic!("the stand-in worker should have run ({error}): {output:?}"))
}

/// A definition's `LongSize` reaches the worker even where it differs from the ABI's default
/// (7.1.1, 7.1.2). The host's default is 8, so 4 is the non-default width.
#[cfg(all(debug_assertions, unix, target_pointer_width = "64"))]
#[test]
fn the_definition_long_size_reaches_the_worker() {
    assert_eq!(
        long_size_passed_to_the_worker("long-size-4", "LongSize = 4\n"),
        "4"
    );
}

/// Without a `LongSize`, the worker gets the ABI's default.
#[cfg(all(debug_assertions, unix, target_pointer_width = "64"))]
#[test]
fn without_long_size_the_worker_gets_the_abi_default() {
    assert_eq!(long_size_passed_to_the_worker("long-size-default", ""), "8");
}

/// `ngr-agent run` takes the per-VCI lock before it resolves the VCI, and waits while another
/// job holds it (ADR-257).
#[cfg(debug_assertions)]
#[test]
fn a_run_waits_for_the_vci_lock() {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use agent::guards::{GuardSetup, JobGuards};

    let temp = TempDir::new("vci-lock");
    let definitions = write_definitions(&temp.0, None);
    let locks = temp.0.join("locks");
    let held = JobGuards::take_vci_only(
        &GuardSetup {
            dir: locks.clone(),
            vci: "test-vci".to_owned(),
        },
        Duration::from_millis(1),
        &AtomicBool::new(false),
    )
    .expect("the test takes the VCI lock");
    let mut child = ngr_agent()
        .args(["run", "--vci", "test-vci", "--mode", "user", "--document"])
        .arg(read_vin(&temp.0))
        .arg("--workers")
        .arg(&temp.0)
        .arg("--locks")
        .arg(&locks)
        .env("NGR_J2534_DEFINITION_DIR", &definitions)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("ngr-agent should start");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().expect("the run's status").is_none(),
        "the run waits for the VCI lock"
    );
    drop(held);
    // With the lock free, the run goes on and fails at the VCI, which no definition names.
    let output = child.wait_with_output().expect("ngr-agent should end");
    assert_fails_with(output, "cannot resolve the library of VCI \"test-vci\"");
}

/// A writing `ngr-agent run` also takes the device's reprogramming slot before it resolves the
/// VCI, so it waits while a job on another VCI holds the slot (ADR-257).
#[cfg(debug_assertions)]
#[test]
fn a_writing_run_waits_for_the_reprogramming_slot() {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use agent::guards::{GuardSetup, JobGuards};

    let temp = TempDir::new("slot");
    let definitions = write_definitions(&temp.0, None);
    let locks = temp.0.join("locks");
    let held = JobGuards::take(
        &GuardSetup {
            dir: locks.clone(),
            vci: "other-vci".to_owned(),
        },
        Duration::from_millis(1),
        &AtomicBool::new(false),
    )
    .expect("the test takes the slot on another VCI");
    // WriteDataByIdentifier is outside the read-only policy.
    let program = write_program(
        &temp.0,
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x2E }],
    );
    let mut child = ngr_agent()
        .args(["run", "--vci", "test-vci", "--mode", "user", "--document"])
        .arg(program)
        .arg("--workers")
        .arg(&temp.0)
        .arg("--locks")
        .arg(&locks)
        .env("NGR_J2534_DEFINITION_DIR", &definitions)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("ngr-agent should start");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().expect("the run's status").is_none(),
        "the run waits for the reprogramming slot"
    );
    drop(held);
    // With the slot free, the run goes on and fails at the VCI, which no definition names.
    let output = child.wait_with_output().expect("ngr-agent should end");
    assert_fails_with(output, "cannot resolve the library of VCI \"test-vci\"");
}

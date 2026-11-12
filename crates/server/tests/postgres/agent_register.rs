//! `ngr-agent register` against a real local-profile server (design 6.4, ADR-463, ADR-465,
//! ADR-470): the agent binary runs as a process with the plain-file key store
//! (`NGR_AGENT_KEY_STORE=file`) and its own `NGR_AGENT_STATE_DIR` and home per child, and the
//! server runs in the test's process over a throwaway PostgreSQL. Codes are issued through the
//! server's own issuance path (`registration::issue`); keys are made by the agent itself.
//!
//! `ngr-agent` belongs to another package, so cargo builds it only when the run builds the
//! `agent` package's binaries (as `cargo test --workspace` and CI do). It is looked up and
//! checked against its sources by `crates/agent/tests/support/ngr_agent_binary.rs`: build it
//! first with `cargo build -p agent --bin ngr-agent` to run this package's tests alone.
//!
//! A release agent has no plain-file store, so this module is compiled in debug builds only.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use server::registration::{self, IssueRequest};
use shared_crypto::{PublicKey, Signer};

use crate::redeem::World;

#[path = "../../../agent/tests/support/ngr_agent_binary.rs"]
mod ngr_agent_binary;

/// The registration record as the agent wrote it: the strict reader is the agent's own and is
/// tested there, so this reads the fields the test compares.
struct Record {
    agent_id: String,
    owner_id: String,
    preset: String,
    allow_remote: bool,
    server_origin: String,
    instruction_key_pem: String,
    instruction_key_id: String,
    agent_key_fingerprint: String,
}

impl Record {
    fn read(path: &Path) -> Result<Record, String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let object = json.as_object().ok_or("not an object")?;
        assert_eq!(object.len(), 8, "the record has its eight fields: {json}");
        let text = |name: &str| {
            json[name]
                .as_str()
                .map(str::to_owned)
                .ok_or(name.to_owned())
        };
        Ok(Record {
            agent_id: text("agent_id")?,
            owner_id: text("owner_id")?,
            preset: text("preset")?,
            allow_remote: json["allow_remote"].as_bool().ok_or("allow_remote")?,
            server_origin: text("server_origin")?,
            instruction_key_pem: text("instruction_key_pem")?,
            instruction_key_id: text("instruction_key_id")?,
            agent_key_fingerprint: text("agent_key_fingerprint")?,
        })
    }
}

/// One agent's surroundings: its home and its state directory.
struct AgentEnv {
    home: tempfile::TempDir,
    state: tempfile::TempDir,
}

impl AgentEnv {
    fn new() -> AgentEnv {
        AgentEnv {
            home: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
        }
    }

    fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(ngr_agent_binary::ngr_agent_path("server"));
        command
            .args(args)
            .env("HOME", self.home.path())
            .env("XDG_STATE_HOME", self.home.path().join("xdg"))
            .env("LOCALAPPDATA", self.home.path().join("local"))
            .env("NGR_AGENT_KEY_STORE", "file")
            .env("NGR_AGENT_STATE_DIR", self.state.path());
        command
    }

    fn record(&self) -> PathBuf {
        self.state
            .path()
            .join("registration")
            .join("registration.v1.json")
    }
}

/// Runs `command` on a blocking thread with `input` on its standard input, so that the server's
/// tasks keep running while it does.
async fn run(mut command: Command, input: String) -> Output {
    tokio::task::spawn_blocking(move || {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(input.as_bytes());
        drop(stdin);
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The agent key's fingerprint; the first call creates the key.
async fn fingerprint(env: &AgentEnv) -> String {
    let args = ["identity", "--mode", "user"].map(str::to_owned).to_vec();
    let output = run(env.command(&args), String::new()).await;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    json["fingerprint"].as_str().unwrap().to_owned()
}

/// A code issued for the agent key `fingerprint`, and the server key ID it was issued under.
async fn issue(world: &World, fingerprint: &str) -> (String, String) {
    let request = IssueRequest::new("alice", "Bench PC", fingerprint, None).unwrap();
    let issued = registration::issue(&world.pool, &request).await.unwrap();
    (issued.code.as_str().to_owned(), issued.server_key_id)
}

fn register_args(world: &World, server_key: &str, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "register",
        "--mode",
        "user",
        "--server",
        &format!("http://127.0.0.1:{}", world.server.addr.port()),
        "--server-key",
        server_key,
    ]
    .map(str::to_owned)
    .to_vec();
    args.extend(extra.iter().map(|a| (*a).to_owned()));
    args
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stderr: {}",
        text(&output.stderr)
    );
}

async fn agents(world: &World) -> String {
    world.sql("SELECT count(*) FROM agents").await
}

fn record_exists(env: &AgentEnv) -> bool {
    Path::new(&env.record()).exists()
}

/// A registration against the real server: the record matches what the server stored, and what
/// the agent printed matches the record.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_registers_with_a_code_and_stores_the_record() {
    let world = World::new().await;
    let (_tenant, owner) = world.add_operator("alice").await;
    let env = AgentEnv::new();
    let fingerprint = fingerprint(&env).await;
    let (code, server_key) = issue(&world, &fingerprint).await;

    let output = run(
        env.command(&register_args(&world, &server_key, &[])),
        format!("{code}\n"),
    )
    .await;
    assert_exit(&output, 0);
    let printed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let record = Record::read(&env.record()).expect("the record is stored and valid");

    // The server stored one agent, with the key the agent holds and the ID the agent recorded.
    assert_eq!(agents(&world).await, "1");
    assert_eq!(
        record.agent_id,
        world.sql("SELECT id::text FROM agents").await
    );
    assert_eq!(record.owner_id, owner);
    assert_eq!(record.preset, "personal");
    assert!(!record.allow_remote);
    assert_eq!(record.agent_key_fingerprint, fingerprint);
    assert_eq!(record.instruction_key_id, server_key);
    assert_eq!(
        record.server_origin,
        format!("http://127.0.0.1:{}", world.server.addr.port())
    );
    let pinned = PublicKey::from_spki_pem(&record.instruction_key_pem).unwrap();
    assert_eq!(pinned.fingerprint_hex(), world.key.key_id());
    assert_eq!(printed["agent_id"], record.agent_id.as_str());
    assert_eq!(printed["replaced"], false);
    assert!(!text(&output.stdout).contains(&code));
    assert!(!text(&output.stderr).contains(&code));

    world.finish().await;
}

/// A wrong code and a reused one are refused (exit 1) and no record is written.
#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_or_reused_code_is_refused_and_writes_no_record() {
    let world = World::new().await;
    world.add_operator("alice").await;
    let env = AgentEnv::new();
    let fingerprint = fingerprint(&env).await;
    let (code, server_key) = issue(&world, &fingerprint).await;

    // A well-formed code that was never issued.
    let never = shared_crypto::registration::RegistrationCode::generate()
        .unwrap()
        .as_str()
        .to_owned();
    let output = run(
        env.command(&register_args(&world, &server_key, &[])),
        never.clone(),
    )
    .await;
    assert_exit(&output, 1);
    assert!(text(&output.stderr).contains("refused: the code is wrong"));
    assert!(!record_exists(&env));
    assert!(!text(&output.stderr).contains(&never));

    // The issued code works once; the second use is refused and the first record stays.
    let ok = run(
        env.command(&register_args(&world, &server_key, &[])),
        code.clone(),
    )
    .await;
    assert_exit(&ok, 0);
    let first = std::fs::read(env.record()).unwrap();
    let again = run(
        env.command(&register_args(&world, &server_key, &["--replace"])),
        code.clone(),
    )
    .await;
    assert_exit(&again, 1);
    assert!(text(&again.stderr).contains("refused: the code is wrong"));
    assert_eq!(std::fs::read(env.record()).unwrap(), first);
    assert_eq!(agents(&world).await, "1");

    world.finish().await;
}

/// K3: a well-formed server key that is not this server's: the server refuses the proof (403,
/// exit 1) and no record is written.
#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_server_key_is_refused_and_writes_no_record() {
    let world = World::new().await;
    world.add_operator("alice").await;
    let env = AgentEnv::new();
    let fingerprint = fingerprint(&env).await;
    let (code, _server_key) = issue(&world, &fingerprint).await;
    let other = shared_crypto::SigningKey::generate()
        .unwrap()
        .public_key()
        .fingerprint_hex();

    let output = run(env.command(&register_args(&world, &other, &[])), code).await;
    assert_exit(&output, 1);
    assert!(text(&output.stderr).contains("--server-key is not this server's"));
    assert!(!record_exists(&env));
    assert_eq!(agents(&world).await, "0");

    world.finish().await;
}

/// An existing record is kept (exit 5) without `--replace`, and a new one replaces it with it. The
/// server gives a fresh agent ID for every registration (ADR-461 Decision item 8).
#[tokio::test(flavor = "multi_thread")]
async fn an_existing_record_needs_replace_and_the_new_one_has_a_new_agent_id() {
    let world = World::new().await;
    world.add_operator("alice").await;
    let env = AgentEnv::new();
    let fingerprint = fingerprint(&env).await;

    let (code1, server_key) = issue(&world, &fingerprint).await;
    let first = run(env.command(&register_args(&world, &server_key, &[])), code1).await;
    assert_exit(&first, 0);
    let old_bytes = std::fs::read(env.record()).unwrap();
    let old = Record::read(&env.record()).unwrap();

    // Without --replace: exit 5, and the unused code is not spent.
    let (code2, _) = issue(&world, &fingerprint).await;
    let refused = run(
        env.command(&register_args(&world, &server_key, &[])),
        code2.clone(),
    )
    .await;
    assert_exit(&refused, 5);
    assert_eq!(std::fs::read(env.record()).unwrap(), old_bytes);
    assert_eq!(agents(&world).await, "1");

    // With --replace: the same code now registers, and the record is new.
    let replaced = run(
        env.command(&register_args(&world, &server_key, &["--replace"])),
        code2,
    )
    .await;
    assert_exit(&replaced, 0);
    let printed: serde_json::Value = serde_json::from_slice(&replaced.stdout).unwrap();
    assert_eq!(printed["replaced"], true);
    let new = Record::read(&env.record()).unwrap();
    assert_ne!(new.agent_id, old.agent_id);
    assert_eq!(new.agent_key_fingerprint, old.agent_key_fingerprint);

    world.finish().await;
}

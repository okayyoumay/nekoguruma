//! Issuing registration codes (ADR-463, ADR-465): `registration::issue` against a throwaway
//! PostgreSQL, and `ngr-server registration-code` as a process. Nothing is seeded by the
//! migrations, so every test inserts the tenant, operator and instruction key rows it needs.
//! Values that look like secrets are made at run time.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use server::db;
use server::registration::{self, IssueError, IssueRequest};
use shared_crypto::random_32;
use shared_crypto::registration::RegistrationCode;
use sqlx::PgPool;

use crate::support::{TestPostgres, refused_url};

const LIMIT: Duration = Duration::from_secs(30);

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A synthetic 64-digit fingerprint of run-time random digits: no key stands behind it.
fn fingerprint() -> String {
    hex(&random_32().expect("random bytes"))
}

/// A migrated database with one enabled operator, `alice`, and an active instruction key.
struct Fixture {
    pg: TestPostgres,
    pool: PgPool,
    key_id: String,
}

impl Fixture {
    async fn new() -> Fixture {
        let f = Fixture::migrated().await;
        f.add_key().await;
        f.add_operator("alice", false).await;
        f
    }

    /// Migrated, with no key row and no operator.
    async fn migrated() -> Fixture {
        let pg = TestPostgres::start();
        let pool = pg.pool();
        db::migrate_with(&pool, &db::MIGRATOR, LIMIT)
            .await
            .expect("the migrations apply");
        Fixture {
            pg,
            pool,
            key_id: fingerprint(),
        }
    }

    async fn add_key(&self) {
        sqlx::query("INSERT INTO instruction_keys (key_id, public_key) VALUES ($1, '\\x00')")
            .bind(&self.key_id)
            .execute(&self.pool)
            .await
            .unwrap();
    }

    /// A new tenant with an operator of that subject.
    async fn add_operator(&self, subject: &str, disabled: bool) {
        sqlx::query(
            "WITH t AS (INSERT INTO tenants (id, name) VALUES (gen_random_uuid(), 'tenant') \
                        RETURNING id) \
             INSERT INTO operators (id, tenant_id, subject, display_name, disabled_at) \
             SELECT gen_random_uuid(), id, $1, 'operator', CASE WHEN $2 THEN now() END FROM t",
        )
        .bind(subject)
        .bind(disabled)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn rows(&self) -> String {
        self.pg
            .scalar("SELECT count(*) FROM agent_registration_codes")
            .await
    }
}

fn request(agent_key: &str, ttl: Option<&str>) -> IssueRequest {
    IssueRequest::new("alice", "Bench PC", agent_key, ttl).unwrap()
}

#[tokio::test]
async fn an_issued_code_is_stored_by_its_hash_with_the_database_clocks_expiry() {
    let f = Fixture::new().await;
    let key = fingerprint();
    let issued = registration::issue(&f.pool, &request(&key.to_ascii_uppercase(), None))
        .await
        .unwrap();

    assert_eq!(issued.server_key_id, f.key_id);
    assert!(RegistrationCode::parse(issued.code.as_str()).is_ok());
    let row = |column: &str| {
        let sql = format!(
            "SELECT {column} FROM agent_registration_codes WHERE code_sha256 = decode('{}', 'hex')",
            hex(&issued.code.sha256())
        );
        let pg = &f.pg;
        async move { pg.scalar(&sql).await }
    };
    assert_eq!(f.rows().await, "1");
    assert_eq!(row("agent_key_id").await, key, "stored in lower case");
    assert_eq!(row("display_name").await, "Bench PC");
    assert_eq!(row("instruction_key_id").await, f.key_id);
    assert_eq!(
        row("owner_id = (SELECT id FROM operators WHERE subject = 'alice')").await,
        "true"
    );
    assert_eq!(
        row("tenant_id = (SELECT tenant_id FROM operators WHERE subject = 'alice')").await,
        "true"
    );
    assert_eq!(
        row("used_at IS NULL AND revoked_at IS NULL AND agent_id IS NULL").await,
        "true"
    );
    assert_eq!(
        row("expires_at - created_at = interval '10 minutes'").await,
        "true"
    );
    assert_eq!(
        row("to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')").await,
        issued.expires_at
    );
    // The text of the code is nowhere in the table.
    assert_eq!(
        f.pg.scalar(&format!(
            "SELECT count(*) FROM agent_registration_codes t WHERE t::text LIKE '%{}%'",
            issued.code.as_str()
        ))
        .await,
        "0"
    );
}

#[tokio::test]
async fn the_lifetime_is_exactly_what_was_asked() {
    let f = Fixture::new().await;
    for minutes in ["1", "60"] {
        let issued = registration::issue(&f.pool, &request(&fingerprint(), Some(minutes)))
            .await
            .unwrap();
        let sql = format!(
            "SELECT expires_at - created_at = interval '{minutes} minutes' \
             FROM agent_registration_codes WHERE code_sha256 = decode('{}', 'hex')",
            hex(&issued.code.sha256())
        );
        assert_eq!(f.pg.scalar(&sql).await, "true", "{minutes} minutes");
    }
}

#[tokio::test]
async fn without_an_active_instruction_key_nothing_is_written() {
    let f = Fixture::migrated().await;
    f.add_operator("alice", false).await;
    let none = registration::issue(&f.pool, &request(&fingerprint(), None)).await;
    assert!(matches!(none, Err(IssueError::NoActiveKey)), "{none:?}");

    f.add_key().await;
    sqlx::query("UPDATE instruction_keys SET retired_at = now()")
        .execute(&f.pool)
        .await
        .unwrap();
    let retired = registration::issue(&f.pool, &request(&fingerprint(), None)).await;
    assert!(
        matches!(retired, Err(IssueError::NoActiveKey)),
        "{retired:?}"
    );
    assert_eq!(f.rows().await, "0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_retired_between_the_check_and_the_insert_refuses_and_writes_nothing() {
    let f = Fixture::new().await;
    // A held table lock stops the issuing statement at its insert, after the key was read.
    let mut holder = f.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE agent_registration_codes IN EXCLUSIVE MODE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let pool = f.pool.clone();
    let issuing =
        tokio::spawn(
            async move { registration::issue(&pool, &request(&fingerprint(), None)).await },
        );
    let waiting = "SELECT count(*) FROM pg_locks WHERE NOT granted \
                   AND relation = 'agent_registration_codes'::regclass";
    let deadline = Instant::now() + LIMIT;
    while f.pg.scalar(waiting).await == "0" {
        assert!(
            Instant::now() < deadline,
            "the insert never waited for the lock"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    sqlx::query("UPDATE instruction_keys SET retired_at = now()")
        .execute(&f.pool)
        .await
        .unwrap();
    holder.rollback().await.unwrap();

    let result = tokio::time::timeout(LIMIT, issuing).await.unwrap().unwrap();
    assert!(matches!(result, Err(IssueError::KeyRetired)), "{result:?}");
    assert_eq!(f.rows().await, "0");
}

#[tokio::test]
async fn an_unknown_disabled_or_ambiguous_owner_is_refused_each_its_own_way() {
    let f = Fixture::migrated().await;
    f.add_key().await;
    let issue = || async { registration::issue(&f.pool, &request(&fingerprint(), None)).await };

    assert!(matches!(issue().await, Err(IssueError::OwnerUnknown)));
    f.add_operator("alice", true).await;
    assert!(matches!(issue().await, Err(IssueError::OwnerDisabled)));
    // A second tenant with the subject, this one enabled: the name no longer picks one operator.
    f.add_operator("alice", false).await;
    assert!(matches!(issue().await, Err(IssueError::OwnerAmbiguous)));
    assert_eq!(f.rows().await, "0");

    let texts = [
        IssueError::OwnerUnknown,
        IssueError::OwnerDisabled,
        IssueError::OwnerAmbiguous,
    ]
    .map(|e| e.to_string());
    assert!(texts[0] != texts[1] && texts[1] != texts[2] && texts[0] != texts[2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_issuances_at_once_make_independent_rows_and_both_codes_stay_valid() {
    let f = Fixture::new().await;
    let key = fingerprint();
    let first = registration::issue(&f.pool, &request(&key, None))
        .await
        .unwrap();
    let again = request(&key, None);
    let (a, b) = tokio::join!(
        registration::issue(&f.pool, &again),
        registration::issue(&f.pool, &again),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a.code, b.code);
    assert_eq!(f.rows().await, "3");
    // An earlier unused code is not touched by a later issuance for the same key.
    assert_eq!(
        f.pg.scalar(&format!(
            "SELECT count(*) FROM agent_registration_codes WHERE agent_key_id = '{key}' \
             AND used_at IS NULL AND revoked_at IS NULL AND expires_at > now()"
        ))
        .await,
        "3"
    );
    assert_ne!(first.code, a.code);
}

// ---------------------------------------------------------------------------------------------
// The command as a process.

/// A configuration file in `dir`: the key path names a file that must never be created.
fn write_config(dir: &Path, database_url: &str) -> PathBuf {
    let path = dir.join("ngr-server.toml");
    std::fs::write(
        &path,
        format!(
            "[instruction_key]\npath = '{}'\n\
             [storage]\ndir = '{}'\n\
             [database]\nurl = \"{database_url}\"\nconnect_timeout_secs = 2\n",
            dir.join("instruction.key").display(),
            dir.join("storage").display()
        ),
    )
    .unwrap();
    path
}

/// Runs `ngr-server registration-code` with the flags of `extra`, bounded by [`LIMIT`].
fn run_command(config: &Path, extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ngr-server"));
    command
        .arg("registration-code")
        .arg("--config")
        .arg(config)
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    let id = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(LIMIT) {
        Ok(output) => output.unwrap(),
        Err(_) => panic!("process {id} did not exit within {LIMIT:?}"),
    }
}

fn issue_flags(key: &str) -> Vec<String> {
    ["--owner", "alice", "--name", "Bench PC", "--agent-key", key]
        .map(String::from)
        .to_vec()
}

fn run_issue(config: &Path, key: &str) -> Output {
    let flags = issue_flags(key);
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    run_command(config, &flags)
}

#[tokio::test]
async fn the_command_prints_one_json_line_that_matches_the_stored_row() {
    let f = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &f.pg.url());
    let key = fingerprint();
    let output = run_issue(&config, &key);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(0), "{stderr}");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.ends_with('\n') && stdout.matches('\n').count() == 1,
        "{stdout:?}"
    );
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let code = json["code"].as_str().unwrap();
    let parsed = RegistrationCode::parse(code).unwrap();
    assert_eq!(parsed.as_str(), code);
    assert_eq!(json["server_key_id"], f.key_id.as_str());
    assert!(!stderr.contains(code), "the code reached stderr: {stderr}");

    let stored =
        f.pg.scalar(&format!(
            "SELECT to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
             FROM agent_registration_codes WHERE code_sha256 = decode('{}', 'hex') \
             AND agent_key_id = '{key}' AND instruction_key_id = '{}'",
            hex(&parsed.sha256()),
            f.key_id
        ))
        .await;
    assert_eq!(json["expires_at"], stored.as_str());
    assert_eq!(f.rows().await, "1");
    // The command checks the database's key row only: the key file is neither read nor created.
    assert!(!dir.path().join("instruction.key").exists());
    assert!(!dir.path().join("storage").exists());
}

#[tokio::test]
async fn the_commands_ttl_flag_sets_the_lifetime() {
    let f = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &f.pg.url());
    let mut flags = issue_flags(&fingerprint());
    flags.extend(["--ttl-minutes".into(), "7".into()]);
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let output = run_command(&config, &flags);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        f.pg.scalar(
            "SELECT count(*) FROM agent_registration_codes \
             WHERE expires_at - created_at = interval '7 minutes'"
        )
        .await,
        "1"
    );
}

#[test]
fn a_usage_error_exits_2_before_any_database_connection() {
    let dir = tempfile::tempdir().unwrap();
    // The URL points at nothing: reaching the connection would take the connect timeout and exit
    // 1, so the exit code 2 shows that parsing stopped first.
    let config = write_config(dir.path(), &refused_url(&crate::support::random_password()));
    let key = fingerprint();
    for flags in [
        vec!["--owner", "alice", "--name", "n"],
        vec!["--owner", "alice", "--name", "n", "--agent-key", "abcd"],
        vec!["--owner", "", "--name", "n", "--agent-key", &key],
        vec!["--owner", "a", "--name", "", "--agent-key", &key],
        vec![
            "--owner",
            "a",
            "--name",
            "n",
            "--agent-key",
            &key,
            "--ttl-minutes",
            "0",
        ],
        vec![
            "--owner",
            "a",
            "--name",
            "n",
            "--agent-key",
            &key,
            "--ttl-minutes",
            "61",
        ],
        vec![
            "--owner",
            "a",
            "--name",
            "n",
            "--agent-key",
            &key,
            "--surprise",
            "1",
        ],
        vec![
            "--owner",
            "a",
            "--owner",
            "b",
            "--name",
            "n",
            "--agent-key",
            &key,
        ],
    ] {
        let started = Instant::now();
        let output = run_command(&config, &flags);
        assert_eq!(output.status.code(), Some(2), "{flags:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("usage:"), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(started.elapsed() < Duration::from_secs(10));
    }
    // The subcommand word is not first: this is the serving form with unknown arguments.
    let output = Command::new(env!("CARGO_BIN_EXE_ngr-server"))
        .args(["--config"])
        .arg(&config)
        .arg("registration-code")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[tokio::test]
async fn without_an_active_key_row_the_command_exits_1_and_writes_nothing() {
    let f = Fixture::migrated().await;
    f.add_operator("alice", false).await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &f.pg.url());
    let output = run_issue(&config, &fingerprint());
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no active instruction key"));
    assert_eq!(f.rows().await, "0");
    assert!(!dir.path().join("instruction.key").exists());
    assert!(!dir.path().join("storage").exists());
}

#[tokio::test]
async fn a_database_that_was_never_migrated_is_refused_and_left_as_it_was() {
    let pg = TestPostgres::start();
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &pg.url());
    let output = run_issue(&config, &fingerprint());
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not been migrated"));
    assert_eq!(
        pg.scalar("SELECT to_regclass('_sqlx_migrations')").await,
        "",
        "the command must not create the migrations table"
    );
}

#[tokio::test]
async fn migrations_that_differ_from_the_binarys_are_refused_each_way() {
    let f = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &f.pg.url());
    let last = db::MIGRATOR.iter().map(|m| m.version).max().unwrap();

    // Behind: the last embedded migration is not recorded.
    let saved =
        f.pg.scalar(&format!(
            "SELECT encode(checksum, 'hex') FROM _sqlx_migrations WHERE version = {last}"
        ))
        .await;
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
        .bind(last)
        .execute(&f.pool)
        .await
        .unwrap();
    let behind = run_issue(&config, &fingerprint());
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES ($1, 'restored', true, decode($2, 'hex'), 0)",
    )
    .bind(last)
    .bind(&saved)
    .execute(&f.pool)
    .await
    .unwrap();

    // Ahead: a version the binary does not carry.
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (9999, 'from a newer binary', true, decode('00', 'hex'), 0)",
    )
    .execute(&f.pool)
    .await
    .unwrap();
    let ahead = run_issue(&config, &fingerprint());
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 9999")
        .execute(&f.pool)
        .await
        .unwrap();

    // A changed checksum.
    sqlx::query("UPDATE _sqlx_migrations SET checksum = decode('00', 'hex') WHERE version = 1")
        .execute(&f.pool)
        .await
        .unwrap();
    let changed = run_issue(&config, &fingerprint());
    sqlx::query("UPDATE _sqlx_migrations SET checksum = decode($1, 'hex') WHERE version = 1")
        .bind(hex(&db::MIGRATOR
            .iter()
            .find(|m| m.version == 1)
            .unwrap()
            .checksum))
        .execute(&f.pool)
        .await
        .unwrap();

    // A half-applied migration.
    sqlx::query("UPDATE _sqlx_migrations SET success = false WHERE version = 2")
        .execute(&f.pool)
        .await
        .unwrap();
    let dirty = run_issue(&config, &fingerprint());
    sqlx::query("UPDATE _sqlx_migrations SET success = true WHERE version = 2")
        .execute(&f.pool)
        .await
        .unwrap();

    for (name, output) in [
        ("behind", behind),
        ("ahead", ahead),
        ("changed", changed),
        ("dirty", dirty),
    ] {
        assert_eq!(output.status.code(), Some(1), "{name}");
        assert!(output.stdout.is_empty(), "{name}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("migrations are not those"),
            "{name}: {stderr}"
        );
    }
    assert_eq!(f.rows().await, "0");
    // With the state restored, the same command succeeds.
    assert_eq!(run_issue(&config, &fingerprint()).status.code(), Some(0));
}

#[test]
fn a_database_that_is_down_exits_1_without_showing_the_url_or_password() {
    let dir = tempfile::tempdir().unwrap();
    let secret = crate::support::random_password();
    let config = write_config(dir.path(), &refused_url(&secret));
    let started = Instant::now();
    let output = run_issue(&config, &fingerprint());
    assert_eq!(output.status.code(), Some(1));
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot connect to the database"),
        "{stderr}"
    );
    assert!(!stderr.contains(&secret), "{stderr}");
    assert!(!stderr.contains("postgres://"), "{stderr}");
}

#[tokio::test]
async fn an_owner_that_cannot_be_used_exits_1_with_its_own_message() {
    let f = Fixture::migrated().await;
    f.add_key().await;
    f.add_operator("alice", true).await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &f.pg.url());
    let output = run_issue(&config, &fingerprint());
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("is disabled"));
    assert_eq!(f.rows().await, "0");
}

#[tokio::test]
async fn a_failed_write_to_standard_output_exits_1_and_leaves_the_row_to_expire() {
    let f = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &f.pg.url());
    let mut command = Command::new(env!("CARGO_BIN_EXE_ngr-server"));
    command
        .arg("registration-code")
        .arg("--config")
        .arg(&config)
        .args(issue_flags(&fingerprint()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    // The reader is gone before the command has connected to the database, let alone written.
    drop(child.stdout.take());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = rx.recv_timeout(LIMIT).expect("the command exits").unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot write the code to standard output"),
        "{stderr}"
    );
    assert_eq!(f.rows().await, "1");
}

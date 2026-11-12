//! The job instruction key at start (ADR-462): the file and the database row that pins it, against
//! a throwaway PostgreSQL. Tests may run as root, so refusals are provoked by what the file holds
//! or by which key the row records, never by an unreadable mode.

use std::path::{Path, PathBuf};
use std::time::Duration;

use server::config::{Config, Database, Http, InstructionKeyConfig, Profile};
use server::instruction_key::KeyError;
use server::{Server, StartError};
use shared_crypto::{Signer, SigningKey};

use crate::support::TestPostgres;

const LIMIT: Duration = Duration::from_secs(30);

fn config(pg: &TestPostgres, key: PathBuf) -> Config {
    let mut database = Database::new(pg.url());
    database.connect_timeout_secs = 5;
    Config {
        profile: Profile::Local,
        http: Http {
            port: 0,
            shutdown_grace_secs: 2,
        },
        database,
        instruction_key: InstructionKeyConfig { path: key },
    }
}

async fn start(pg: &TestPostgres, key: &Path) -> Result<Server, StartError> {
    tokio::time::timeout(LIMIT, Server::start(&config(pg, key.to_owned())))
        .await
        .expect("Server::start must not hang")
}

async fn stop(server: Server) {
    server.run(async {}, std::future::pending()).await.unwrap();
}

/// The key ID of the key file at `path`.
fn file_key_id(path: &Path) -> String {
    SigningKey::from_pkcs8_der(&std::fs::read(path).unwrap())
        .unwrap()
        .public_key()
        .fingerprint_hex()
}

/// Writes a new key of its own to `path`, readable by the owner only; returns its key ID.
fn write_new_key(path: &Path) -> String {
    let key = SigningKey::generate().unwrap();
    std::fs::write(path, &*key.to_pkcs8_der()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    key.public_key().fingerprint_hex()
}

fn refused(result: Result<Server, StartError>) -> KeyError {
    match result {
        Err(StartError::InstructionKey(error)) => error,
        Err(other) => panic!("a key error was expected, not {other}"),
        Ok(_) => panic!("the start must be refused"),
    }
}

const ACTIVE_ID: &str =
    "SELECT string_agg(key_id, ',') FROM instruction_keys WHERE retired_at IS NULL";

#[tokio::test]
async fn the_first_start_creates_the_key_and_records_its_public_half() {
    let pg = TestPostgres::start();
    let path = pg.key_path();
    assert!(!path.exists());
    stop(start(&pg, &path).await.unwrap()).await;
    assert!(path.is_file());
    assert_eq!(pg.scalar(ACTIVE_ID).await, file_key_id(&path));
    assert_eq!(
        pg.scalar("SELECT count(*) FROM instruction_keys").await,
        "1"
    );
}

#[tokio::test]
async fn a_restart_keeps_the_key_and_the_row() {
    let pg = TestPostgres::start();
    let path = pg.key_path();
    stop(start(&pg, &path).await.unwrap()).await;
    let bytes = std::fs::read(&path).unwrap();
    let id = pg.scalar(ACTIVE_ID).await;
    stop(start(&pg, &path).await.unwrap()).await;
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(pg.scalar(ACTIVE_ID).await, id);
    assert_eq!(
        pg.scalar("SELECT count(*) FROM instruction_keys").await,
        "1"
    );
}

#[tokio::test]
async fn a_valid_file_without_a_row_is_recorded() {
    let pg = TestPostgres::start();
    let path = pg.key_path();
    let id = write_new_key(&path);
    stop(start(&pg, &path).await.unwrap()).await;
    assert_eq!(pg.scalar(ACTIVE_ID).await, id);
}

#[tokio::test]
async fn a_removed_file_with_a_recorded_key_refuses_the_start_and_is_not_recreated() {
    let pg = TestPostgres::start();
    let path = pg.key_path();
    stop(start(&pg, &path).await.unwrap()).await;
    let id = pg.scalar(ACTIVE_ID).await;
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(
        refused(start(&pg, &path).await),
        KeyError::FileMissing
    ));
    assert!(!path.exists(), "a missing key file is never regenerated");
    assert_eq!(pg.scalar(ACTIVE_ID).await, id);
}

#[tokio::test]
async fn a_different_key_file_refuses_the_start_and_both_stay_as_they_were() {
    let pg = TestPostgres::start();
    let path = pg.key_path();
    stop(start(&pg, &path).await.unwrap()).await;
    let id = pg.scalar(ACTIVE_ID).await;

    let other_dir = tempfile::tempdir().unwrap();
    let other = other_dir.path().join("other.key");
    let other_id = write_new_key(&other);
    assert_ne!(other_id, id);
    let before = std::fs::read(&other).unwrap();
    assert!(matches!(
        refused(start(&pg, &other).await),
        KeyError::Differs
    ));
    assert_eq!(std::fs::read(&other).unwrap(), before);
    assert_eq!(pg.scalar(ACTIVE_ID).await, id);
    assert_eq!(
        pg.scalar("SELECT count(*) FROM instruction_keys").await,
        "1"
    );
}

#[tokio::test]
async fn a_file_that_is_not_a_key_refuses_the_start_and_is_not_overwritten() {
    let pg = TestPostgres::start();
    let path = pg.key_path();
    std::fs::write(&path, b"not a key").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(matches!(
        refused(start(&pg, &path).await),
        KeyError::NotAKey
    ));
    assert_eq!(std::fs::read(&path).unwrap(), b"not a key");
    assert_eq!(
        pg.scalar("SELECT count(*) FROM instruction_keys").await,
        "0"
    );
}

#[tokio::test]
async fn a_retired_record_without_an_active_one_refuses_a_key_that_is_not_new() {
    let pg = TestPostgres::start();
    let path = pg.key_path();
    stop(start(&pg, &path).await.unwrap()).await;
    pg.execute("UPDATE instruction_keys SET retired_at = now()")
        .await;
    // The file is the retired key: it is not recorded again.
    assert!(matches!(
        refused(start(&pg, &path).await),
        KeyError::Retired
    ));
    // After the documented reset (the file removed too), a new key is created and recorded.
    std::fs::remove_file(&path).unwrap();
    stop(start(&pg, &path).await.unwrap()).await;
    assert_eq!(
        pg.scalar("SELECT count(*) FROM instruction_keys").await,
        "2"
    );
    assert_eq!(pg.scalar(ACTIVE_ID).await, file_key_id(&path));
}

#[tokio::test]
async fn the_database_allows_one_active_key() {
    let pg = TestPostgres::start();
    stop(start(&pg, &pg.key_path()).await.unwrap()).await;
    let pool = pg.pool();
    let second = sqlx::query(
        "INSERT INTO instruction_keys (key_id, public_key) VALUES ('second', decode('00', 'hex'))",
    )
    .execute(&pool)
    .await;
    let error = second.expect_err("a second active row must be refused");
    let unique_violation = error
        .as_database_error()
        .and_then(|e| e.code().map(|c| c.into_owned()));
    assert_eq!(unique_violation.as_deref(), Some("23505"), "{error}");
    // A retired row is history, and any number of them may exist.
    for id in ["old-1", "old-2"] {
        sqlx::query(
            "INSERT INTO instruction_keys (key_id, public_key, retired_at) \
             VALUES ($1, decode('00', 'hex'), now())",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_starts_with_one_path_both_succeed_with_one_key() {
    let pg = TestPostgres::start();
    let path = pg.key_path();
    let spawn = || {
        let config = config(&pg, path.clone());
        tokio::spawn(async move { Server::start(&config).await })
    };
    let (a, b) = (spawn(), spawn());
    let (a, b) = tokio::time::timeout(LIMIT, async { (a.await.unwrap(), b.await.unwrap()) })
        .await
        .expect("both starts finish");
    let (a, b) = (a.expect("the first start"), b.expect("the second start"));
    assert_eq!(
        pg.scalar("SELECT count(*) FROM instruction_keys").await,
        "1"
    );
    assert_eq!(pg.scalar(ACTIVE_ID).await, file_key_id(&path));
    stop(a).await;
    stop(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_starts_with_different_paths_exactly_one_succeeds() {
    let pg = TestPostgres::start();
    let other_dir = tempfile::tempdir().unwrap();
    let paths = [pg.key_path(), other_dir.path().join("instruction.key")];
    let spawn = |path: &PathBuf| {
        let config = config(&pg, path.clone());
        tokio::spawn(async move { Server::start(&config).await })
    };
    let (a, b) = (spawn(&paths[0]), spawn(&paths[1]));
    let (a, b) = tokio::time::timeout(LIMIT, async { (a.await.unwrap(), b.await.unwrap()) })
        .await
        .expect("both starts finish");
    let mut winners = 0;
    for result in [a, b] {
        match result {
            Ok(server) => {
                winners += 1;
                stop(server).await;
            }
            // The loser either finds the winner's row before it creates a file of its own, or
            // creates one and finds the row records another key.
            Err(StartError::InstructionKey(KeyError::FileMissing | KeyError::Differs)) => {}
            Err(other) => panic!("unexpected failure: {other}"),
        }
    }
    assert_eq!(winners, 1, "one deployment, one key");
    assert_eq!(
        pg.scalar("SELECT count(*) FROM instruction_keys").await,
        "1"
    );
}

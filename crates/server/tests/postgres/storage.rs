//! The object store at start (ADR-474): `Server::start` opens it after the instruction key and
//! before it binds, against a throwaway PostgreSQL. The store's own behaviour is tested in
//! `crates/server/src/storage/local/tests.rs`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use server::config::{Config, Database, Http, InstructionKeyConfig, Profile, StorageConfig};
use server::storage::{ObjectStatus, StoreError};
use server::{Server, StartError};

use crate::support::TestPostgres;

const LIMIT: Duration = Duration::from_secs(30);

fn config(pg: &TestPostgres, storage: &Path) -> Config {
    let mut database = Database::new(pg.url());
    database.connect_timeout_secs = 5;
    Config {
        profile: Profile::Local,
        http: Http {
            port: 0,
            shutdown_grace_secs: 2,
        },
        database,
        instruction_key: InstructionKeyConfig {
            path: pg.key_path(),
        },
        storage: StorageConfig::new(storage),
    }
}

async fn start(pg: &TestPostgres, storage: &Path) -> Result<Server, StartError> {
    tokio::time::timeout(LIMIT, Server::start(&config(pg, storage)))
        .await
        .expect("Server::start must not hang")
}

async fn stop(server: Server) {
    server.run(async {}, std::future::pending()).await.unwrap();
}

fn refused(result: Result<Server, StartError>) -> StoreError {
    match result {
        Err(StartError::Storage(error)) => error,
        Err(other) => panic!("a storage error was expected, not {other}"),
        Ok(_) => panic!("the start must be refused"),
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}

#[tokio::test]
async fn a_start_creates_the_directory_binds_and_serves_the_store() {
    let pg = TestPostgres::start();
    let dir = pg.storage_path();
    let server = start(&pg, &dir).await.unwrap();
    assert!(dir.join("staging").is_dir() && dir.join("objects").is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("staging")), 0o700);
        assert_eq!(mode(&dir.join(".lock")), 0o600);
    }
    assert_ne!(server.local_addr().port(), 0);
    let store = server.object_store().expect("start opens the store");
    let key = store.begin().await.unwrap();
    assert_eq!(store.append(&key, 0, b"abc".to_vec()).await.unwrap(), 3);
    store.finish(&key, sha256(b"abc"), 3).await.unwrap();
    stop(server).await;
    // The store's lock is held until the last handle to it is gone.
    drop(store);
    // A restart finds the object where it was left.
    let server = start(&pg, &dir).await.unwrap();
    let store = server.object_store().unwrap();
    assert_eq!(store.status(&key).await.unwrap(), ObjectStatus::Finished);
    stop(server).await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_group_writable_directory_refuses_the_start() {
    use std::os::unix::fs::PermissionsExt;
    let pg = TestPostgres::start();
    let dir = pg.storage_path();
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(matches!(
        refused(start(&pg, &dir).await),
        StoreError::DirectoryWritable
    ));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[tokio::test]
async fn a_missing_parent_refuses_the_start_and_creates_nothing() {
    let pg = TestPostgres::start();
    let parent: PathBuf = pg.storage_path();
    let dir = parent.join("store");
    assert!(matches!(
        refused(start(&pg, &dir).await),
        StoreError::Io {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    ));
    assert!(!parent.exists());
}

#[tokio::test]
async fn a_second_server_on_one_directory_is_refused_until_the_first_has_stopped() {
    let pg = TestPostgres::start();
    let dir = pg.storage_path();
    let first = start(&pg, &dir).await.unwrap();
    assert!(matches!(refused(start(&pg, &dir).await), StoreError::InUse));
    stop(first).await;
    stop(start(&pg, &dir).await.unwrap()).await;
}

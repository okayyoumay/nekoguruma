//! `Server::start` against a throwaway PostgreSQL: the embedded migrations are applied before the
//! listener binds, a second start is a no-op, a changed or unknown migration refuses the start
//! without hanging, and two servers starting at once both succeed (ADR-458).

use std::time::Duration;

use server::config::{Config, Database, Http, InstructionKeyConfig, Profile};
use server::db::{self, DbError};
use server::{Server, StartError};
use sqlx::migrate::{Migration, MigrationType, Migrator};

use crate::support::{TestPostgres, free_port};

/// Longer than any start below should take; a hang fails the test instead of the run.
const LIMIT: Duration = Duration::from_secs(30);

fn config(pg: &TestPostgres) -> Config {
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
    }
}

async fn start(pg: &TestPostgres) -> Result<Server, StartError> {
    tokio::time::timeout(LIMIT, Server::start(&config(pg)))
        .await
        .expect("Server::start must not hang")
}

/// Ends a server and closes its pool.
async fn stop(server: Server) {
    server.run(async {}, std::future::pending()).await.unwrap();
}

fn migration_error(result: Result<Server, StartError>) -> sqlx::migrate::MigrateError {
    match result {
        Err(StartError::Database(DbError::Migrate(error))) => error,
        Err(other) => panic!("a migration error was expected, not {other}"),
        Ok(_) => panic!("the start must be refused"),
    }
}

#[tokio::test]
async fn migrations_apply_to_an_empty_database_and_a_second_start_is_a_no_op() {
    let pg = TestPostgres::start();
    let embedded = db::MIGRATOR.iter().count();
    assert_eq!(
        pg.scalar("SELECT to_regclass('_sqlx_migrations')").await,
        "",
        "the cluster starts empty"
    );

    stop(start(&pg).await.unwrap()).await;
    let rows = "SELECT count(*) FROM _sqlx_migrations WHERE success";
    assert_eq!(pg.scalar(rows).await, embedded.to_string());
    // A table of the first migration exists.
    assert_eq!(pg.scalar("SELECT to_regclass('jobs')").await, "jobs");
    let applied = "SELECT string_agg(installed_on::text, ',' ORDER BY version) \
                   FROM _sqlx_migrations";
    let first = pg.scalar(applied).await;

    stop(start(&pg).await.unwrap()).await;
    assert_eq!(pg.scalar(rows).await, embedded.to_string());
    assert_eq!(
        pg.scalar(applied).await,
        first,
        "a second start changes nothing"
    );
}

#[tokio::test]
async fn an_applied_migration_that_has_changed_refuses_the_start() {
    let pg = TestPostgres::start();
    stop(start(&pg).await.unwrap()).await;
    pg.execute("UPDATE _sqlx_migrations SET checksum = decode('00', 'hex') WHERE version = 1")
        .await;
    let error = migration_error(start(&pg).await);
    assert!(
        matches!(error, sqlx::migrate::MigrateError::VersionMismatch(1)),
        "{error}"
    );
}

#[tokio::test]
async fn a_database_with_a_migration_the_binary_lacks_refuses_the_start() {
    let pg = TestPostgres::start();
    stop(start(&pg).await.unwrap()).await;
    pg.execute(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (9999, 'from a newer binary', true, decode('00', 'hex'), 0)",
    )
    .await;
    let error = migration_error(start(&pg).await);
    assert!(
        matches!(error, sqlx::migrate::MigrateError::VersionMissing(9999)),
        "{error}"
    );
}

#[tokio::test]
async fn a_migration_that_was_left_half_applied_refuses_the_start() {
    let pg = TestPostgres::start();
    stop(start(&pg).await.unwrap()).await;
    pg.execute("UPDATE _sqlx_migrations SET success = false WHERE version = 2")
        .await;
    let error = migration_error(start(&pg).await);
    assert!(
        matches!(error, sqlx::migrate::MigrateError::Dirty(2)),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_servers_starting_together_against_an_empty_database_both_succeed() {
    let pg = TestPostgres::start();
    let config = config(&pg);
    let spawn = || {
        let config = config.clone();
        tokio::spawn(async move { Server::start(&config).await })
    };
    let (a, b) = (spawn(), spawn());
    let (a, b) = tokio::time::timeout(LIMIT, async { (a.await.unwrap(), b.await.unwrap()) })
        .await
        .expect("both starts finish");
    let (a, b) = (a.expect("the first start"), b.expect("the second start"));
    let rows = "SELECT count(*) FROM _sqlx_migrations WHERE success";
    assert_eq!(
        pg.scalar(rows).await,
        db::MIGRATOR.iter().count().to_string()
    );
    stop(a).await;
    stop(b).await;
}

#[tokio::test]
async fn a_start_against_a_database_that_is_down_fails_within_the_connect_timeout() {
    let mut database = Database::new(format!(
        "postgres://ngr:pw@127.0.0.1:{}/nekoguruma",
        free_port()
    ));
    database.connect_timeout_secs = 1;
    let config = Config {
        profile: Profile::Local,
        http: Http {
            port: 0,
            shutdown_grace_secs: 2,
        },
        database,
        instruction_key: InstructionKeyConfig {
            path: std::env::temp_dir().join("never-created.key"),
        },
    };
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(LIMIT, Server::start(&config))
        .await
        .expect("Server::start must not hang");
    assert!(
        matches!(result, Err(StartError::Database(DbError::Connect { .. }))),
        "the start must fail on the connection"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

fn migrator(sql: &'static str) -> Migrator {
    let migration = Migration::new(
        1,
        "test".into(),
        MigrationType::Simple,
        sqlx::SqlStr::from_static(sql),
        false,
    );
    Migrator::with_migrations(vec![migration])
}

#[tokio::test]
async fn a_migration_that_runs_too_long_is_stopped_by_the_server_and_frees_the_lock() {
    let pg = TestPostgres::start();
    let pool = pg.pool();
    let started = std::time::Instant::now();
    let result = db::migrate_with(
        &pool,
        &migrator("SELECT pg_sleep(60)"),
        Duration::from_secs(1),
    )
    .await;
    // PostgreSQL ends the statement at the limit, ahead of the client's own margin.
    assert!(matches!(result, Err(DbError::Migrate(_))), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    db::close(&pool).await;

    // The lock is gone: another start (a new pool, as after a restart) goes through at once.
    let next = pg.pool();
    let started = std::time::Instant::now();
    db::migrate_with(
        &next,
        &migrator("CREATE TABLE after_timeout (id int)"),
        Duration::from_secs(5),
    )
    .await
    .expect("the lock was released");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        pg.scalar("SELECT to_regclass('after_timeout')").await,
        "after_timeout"
    );
    db::close(&next).await;
}

#[tokio::test]
async fn a_migration_whose_sql_fails_is_rolled_back_and_a_fixed_one_applies() {
    let pg = TestPostgres::start();
    let pool = pg.pool();
    let broken = migrator("CREATE TABLE half_done (id int); SELECT 1 / 0;");
    let result = db::migrate_with(&pool, &broken, Duration::from_secs(5)).await;
    assert!(
        matches!(
            result,
            Err(DbError::Migrate(
                sqlx::migrate::MigrateError::ExecuteMigration(_, 1)
            ))
        ),
        "{result:?}"
    );
    db::close(&pool).await;
    assert_eq!(
        pg.scalar("SELECT to_regclass('half_done')").await,
        "",
        "rolled back"
    );
    assert_eq!(
        pg.scalar("SELECT count(*) FROM _sqlx_migrations").await,
        "0"
    );

    let pool = pg.pool();
    db::migrate_with(
        &pool,
        &migrator("CREATE TABLE half_done (id int)"),
        Duration::from_secs(5),
    )
    .await
    .expect("the corrected migration applies");
    assert_eq!(
        pg.scalar("SELECT to_regclass('half_done')").await,
        "half_done"
    );
    db::close(&pool).await;
}

/// A migrator holding the embedded migrations up to and including `version`.
fn migrator_up_to(version: i64) -> Migrator {
    Migrator::with_migrations(
        db::MIGRATOR
            .iter()
            .filter(|m| m.version <= version)
            .cloned()
            .collect::<Vec<_>>(),
    )
}

#[tokio::test]
async fn the_agent_mode_machine_becomes_device_and_the_check_follows() {
    let pg = TestPostgres::start();
    let pool = pg.pool();
    db::migrate_with(&pool, &migrator_up_to(3), Duration::from_secs(30))
        .await
        .expect("migrations 1 to 3 apply");
    pg.execute("INSERT INTO tenants (id, name) VALUES (gen_random_uuid(), 'rename test')")
        .await;
    pg.execute(
        "INSERT INTO agents (id, tenant_id, display_name, mode, public_key) \
         SELECT gen_random_uuid(), id, 'old machine agent', 'machine', '\\x01' FROM tenants",
    )
    .await;
    pg.execute(
        "INSERT INTO agents (id, tenant_id, display_name, mode, public_key) \
         SELECT gen_random_uuid(), id, 'user agent', 'user', '\\x02' FROM tenants",
    )
    .await;

    db::migrate_with(&pool, &db::MIGRATOR, Duration::from_secs(30))
        .await
        .expect("the rest apply over existing rows");
    db::close(&pool).await;

    let modes = "SELECT string_agg(display_name || '=' || mode, ',' ORDER BY display_name) \
                 FROM agents";
    assert_eq!(
        pg.scalar(modes).await,
        "old machine agent=device,user agent=user"
    );

    // The check now allows device and user and refuses machine.
    // Each probe has a key of its own: one unrevoked row per key is allowed (migration 0005).
    for (n, (mode, ok)) in [("device", true), ("user", true), ("machine", false)]
        .into_iter()
        .enumerate()
    {
        let sql = format!(
            "INSERT INTO agents (id, tenant_id, display_name, mode, public_key) \
             SELECT gen_random_uuid(), id, 'probe', '{mode}', '\\x0{}' FROM tenants",
            n + 3
        );
        let probe = pg.pool();
        let outcome = sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&probe).await;
        db::close(&probe).await;
        assert_eq!(outcome.is_ok(), ok, "mode {mode}: {outcome:?}");
    }
}

/// Applies migrations 1 to 4, runs `seed` (statements of fixed text), then applies the rest, as a
/// server upgrading a database that already holds agents does.
async fn upgrade_from_4(pg: &TestPostgres, seed: &[&'static str]) {
    let pool = pg.pool();
    db::migrate_with(&pool, &migrator_up_to(4), Duration::from_secs(30))
        .await
        .expect("migrations 1 to 4 apply");
    for sql in seed {
        pg.execute(sql).await;
    }
    db::migrate_with(&pool, &db::MIGRATOR, Duration::from_secs(30))
        .await
        .expect("migration 5 applies over the seeded rows");
    db::close(&pool).await;
}

const TWO_TENANTS: &str = "INSERT INTO tenants (id, name) VALUES \
    ('00000000-0000-0000-0000-0000000000a1', 'first'), \
    ('00000000-0000-0000-0000-0000000000a2', 'second')";

const UNREVOKED_OF_KEY_A: &str = "SELECT string_agg(id::text, ',' ORDER BY id) FROM agents \
    WHERE public_key = '\\x0a' AND revoked_at IS NULL";

#[tokio::test]
async fn distinct_keys_are_left_alone_and_owner_id_starts_null() {
    let pg = TestPostgres::start();
    upgrade_from_4(
        &pg,
        &[
            TWO_TENANTS,
            "INSERT INTO agents (id, tenant_id, display_name, mode, public_key) VALUES \
             ('00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-0000000000a1', \
              'one', 'user', '\\x01'), \
             ('00000000-0000-0000-0000-000000000002', '00000000-0000-0000-0000-0000000000a2', \
              'two', 'device', '\\x02')",
        ],
    )
    .await;
    assert_eq!(
        pg.scalar("SELECT count(*) FROM agents WHERE revoked_at IS NULL AND owner_id IS NULL")
            .await,
        "2"
    );
    assert_eq!(
        pg.scalar("SELECT count(*) FROM agents WHERE revoked_at IS NOT NULL")
            .await,
        "0"
    );
}

#[tokio::test]
async fn duplicate_unrevoked_rows_of_a_key_keep_only_the_newest_and_the_index_follows() {
    let pg = TestPostgres::start();
    // The key's rows are in two tenants; the newest is in the second.
    upgrade_from_4(
        &pg,
        &[
            TWO_TENANTS,
            "INSERT INTO agents (id, tenant_id, display_name, mode, public_key, registered_at) \
             VALUES \
             ('00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-0000000000a1', \
              'old', 'user', '\\x0a', '2026-01-01T00:00:00Z'), \
             ('00000000-0000-0000-0000-000000000003', '00000000-0000-0000-0000-0000000000a2', \
              'newest', 'user', '\\x0a', '2026-01-03T00:00:00Z'), \
             ('00000000-0000-0000-0000-000000000002', '00000000-0000-0000-0000-0000000000a1', \
              'middle', 'user', '\\x0a', '2026-01-02T00:00:00Z'), \
             ('00000000-0000-0000-0000-000000000009', '00000000-0000-0000-0000-0000000000a1', \
              'other key', 'user', '\\x0b', '2026-01-01T00:00:00Z')",
        ],
    )
    .await;
    assert_eq!(
        pg.scalar(UNREVOKED_OF_KEY_A).await,
        "00000000-0000-0000-0000-000000000003"
    );
    assert_eq!(
        pg.scalar("SELECT count(*) FROM agents WHERE public_key = '\\x0b' AND revoked_at IS NULL")
            .await,
        "1"
    );
    // The index is in place and refuses a second unrevoked row, in any tenant.
    let pool = pg.pool();
    let second = sqlx::query(
        "INSERT INTO agents (id, tenant_id, display_name, mode, public_key) VALUES \
         (gen_random_uuid(), '00000000-0000-0000-0000-0000000000a1', 'again', 'user', '\\x0a')",
    )
    .execute(&pool)
    .await;
    assert!(second.is_err(), "a second unrevoked row of one key");
    db::close(&pool).await;
}

#[tokio::test]
async fn equal_registration_times_are_broken_by_the_larger_id() {
    let pg = TestPostgres::start();
    upgrade_from_4(
        &pg,
        &[
            TWO_TENANTS,
            "INSERT INTO agents (id, tenant_id, display_name, mode, public_key, registered_at) \
             VALUES \
             ('00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-0000000000a1', \
              'a', 'user', '\\x0a', '2026-01-01T00:00:00Z'), \
             ('00000000-0000-0000-0000-00000000000f', '00000000-0000-0000-0000-0000000000a1', \
              'b', 'user', '\\x0a', '2026-01-01T00:00:00Z'), \
             ('00000000-0000-0000-0000-000000000005', '00000000-0000-0000-0000-0000000000a1', \
              'c', 'user', '\\x0a', '2026-01-01T00:00:00Z')",
        ],
    )
    .await;
    assert_eq!(
        pg.scalar(UNREVOKED_OF_KEY_A).await,
        "00000000-0000-0000-0000-00000000000f"
    );
}

#[tokio::test]
async fn already_revoked_duplicates_are_untouched() {
    let pg = TestPostgres::start();
    upgrade_from_4(
        &pg,
        &[
            TWO_TENANTS,
            "INSERT INTO agents \
                 (id, tenant_id, display_name, mode, public_key, registered_at, revoked_at) \
             VALUES \
             ('00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-0000000000a1', \
              'revoked early', 'user', '\\x0a', '2026-01-01T00:00:00Z', '2026-02-01T00:00:00Z'), \
             ('00000000-0000-0000-0000-000000000002', '00000000-0000-0000-0000-0000000000a1', \
              'revoked later', 'user', '\\x0a', '2026-01-02T00:00:00Z', '2026-02-02T00:00:00Z'), \
             ('00000000-0000-0000-0000-000000000003', '00000000-0000-0000-0000-0000000000a1', \
              'live', 'user', '\\x0a', '2026-01-03T00:00:00Z', NULL)",
        ],
    )
    .await;
    assert_eq!(
        pg.scalar(UNREVOKED_OF_KEY_A).await,
        "00000000-0000-0000-0000-000000000003"
    );
    assert_eq!(
        pg.scalar(
            "SELECT string_agg(revoked_at::date::text, ',' ORDER BY id) FROM agents \
             WHERE revoked_at IS NOT NULL"
        )
        .await,
        "2026-02-01,2026-02-02"
    );
}

/// A tracing writer that keeps what it is given.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn the_migration_reports_how_many_duplicate_rows_it_revoked() {
    let pg = TestPostgres::start();
    let captured = Captured::default();
    let writer = captured.clone();
    // PostgreSQL's notices reach tracing from sqlx, on the target below, in the task that runs the
    // statement; the default subscriber of this thread covers the whole test (current-thread
    // runtime).
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    crate::support::pin_dispatchers();
    let _guard = tracing::subscriber::set_default(subscriber);
    upgrade_from_4(
        &pg,
        &[
            TWO_TENANTS,
            "INSERT INTO agents (id, tenant_id, display_name, mode, public_key, registered_at) \
             VALUES \
             ('00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-0000000000a1', \
              'a', 'user', '\\x0a', '2026-01-01T00:00:00Z'), \
             ('00000000-0000-0000-0000-000000000002', '00000000-0000-0000-0000-0000000000a2', \
              'b', 'user', '\\x0a', '2026-01-02T00:00:00Z'), \
             ('00000000-0000-0000-0000-000000000003', '00000000-0000-0000-0000-0000000000a1', \
              'c', 'user', '\\x0a', '2026-01-03T00:00:00Z')",
        ],
    )
    .await;
    let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("sqlx::postgres::notice")
            && text.contains("revoked 2 duplicate unrevoked agent rows"),
        "{text}"
    );
}

//! The database: the connection pool and the embedded migrations (design 12, ADR-458).

use std::time::Duration;

use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;

use crate::config::{ConfigError, Database};

/// The migrations of `db/migrations`, embedded when the crate is built (design 14: one binary).
/// `build.rs` makes cargo watch the directory.
pub static MIGRATOR: Migrator = sqlx::migrate!("../../db/migrations");

/// The longest `ngr-server` waits for the migrations at start.
pub const MIGRATION_TIMEOUT: Duration = Duration::from_secs(60);

/// The table in which sqlx records applied migrations. [`require_current`] reads it by this name;
/// a test pins it against [`MIGRATOR`]'s own table name.
const MIGRATIONS_TABLE: &str = "_sqlx_migrations";

/// How long closing the pool may take before the server gives up waiting for it.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The most connections the pool opens. The server's own load is small; the bound keeps it well
/// under PostgreSQL's default connection limit when two instances run (design 15).
const MAX_CONNECTIONS: u32 = 10;

/// Why the database could not be made ready.
///
/// Neither variant shows the configured URL: sqlx's messages do not repeat it, and the
/// configuration's own error for a bad URL is fixed text (ADR-458).
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("cannot connect to the database within {secs} s: {source}")]
    Connect { secs: u64, source: sqlx::Error },
    #[error("the migrations failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("the migrations did not finish within {0} s plus the client's margin")]
    MigrateTimeout(u64),
    #[error("the database has not been migrated; start ngr-server once to apply the migrations")]
    NotMigrated,
    #[error("the database's migrations are not those of this ngr-server: {0}")]
    MigrationsDiffer(MigrationsDiffer),
    #[error("cannot read the database's migration state: {0}")]
    MigrationState(sqlx::Error),
}

/// What a failed statement means to a route: the database is unavailable (503), or this code has
/// a fault (500). The text is coarse and never the error's own, since a constraint failure's
/// detail can name a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    Unavailable(String),
    Internal(String),
}

/// A pool, connection or database-execution failure (including a lock or statement timeout and
/// a serialization failure) is unavailability. A fault in this code (a decode, encode, column or
/// type mismatch, a missing row) or any other kind is internal.
pub(crate) fn failure(error: &sqlx::Error) -> Failure {
    match error {
        sqlx::Error::Database(db) => Failure::Unavailable(match db.code() {
            Some(code) => format!("database error, SQLSTATE {code}"),
            None => "database error".to_owned(),
        }),
        sqlx::Error::PoolTimedOut => {
            Failure::Unavailable("no connection available in time".to_owned())
        }
        sqlx::Error::PoolClosed => Failure::Unavailable("the pool is closed".to_owned()),
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::Protocol(_)
        | sqlx::Error::WorkerCrashed
        | sqlx::Error::BeginFailed => Failure::Unavailable("connection failed".to_owned()),
        _ => Failure::Internal("unexpected database client error".to_owned()),
    }
}

/// Sets the lock and statement timeouts (5 seconds) for the transaction `tx`, so that a read or
/// write that waits on a lock or runs long fails as unavailability instead of hanging a request.
pub(crate) async fn statement_timeouts(tx: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT set_config('lock_timeout', '5s', true), \
                set_config('statement_timeout', '5s', true)",
    )
    .execute(tx)
    .await
    .map(|_| ())
}

/// How the applied migrations differ from the embedded ones (see [`require_current`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MigrationsDiffer {
    #[error("an embedded migration is not applied yet (the database is behind)")]
    Behind,
    #[error("the database holds a migration this binary does not carry (the database is ahead)")]
    Ahead,
    #[error("an applied migration differs from the embedded file")]
    Changed,
    #[error("a migration was left half applied")]
    Dirty,
}

/// Opens the pool, with the connect timeout as the pool's acquire timeout: sqlx retries a refused
/// connection until it elapses, and the first connection is made here, so a database that is
/// down is reported within the timeout and not at the first request.
pub async fn connect(config: &Database) -> Result<PgPool, DbError> {
    let options = config.connect_options()?;
    let timeout = Duration::from_secs(config.connect_timeout_secs);
    PgPoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .acquire_timeout(timeout)
        .connect_with(options)
        .await
        .map_err(|source| DbError::Connect {
            secs: config.connect_timeout_secs,
            source,
        })
}

/// Checks, read-only, that the database's applied migrations equal the embedded ones, both ways:
/// every embedded migration is applied with its checksum, and the database holds nothing else.
///
/// The check never writes: `Migrator::run_direct` would create the migrations table, and a
/// command that must not migrate (ADR-458, ADR-465) cannot use it. It reads committed state and
/// takes no lock, so a server that is applying migrations at that moment makes it fail; a rerun
/// after the start finishes passes.
pub async fn require_current(pool: &PgPool) -> Result<(), DbError> {
    let present: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(MIGRATIONS_TABLE)
        .fetch_one(pool)
        .await
        .map_err(DbError::MigrationState)?;
    if present.is_none() {
        return Err(DbError::NotMigrated);
    }
    let sql = format!("SELECT version, checksum, success FROM {MIGRATIONS_TABLE} ORDER BY version");
    let applied: Vec<(i64, Vec<u8>, bool)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .map_err(DbError::MigrationState)?;
    compare_migrations(&MIGRATOR, &applied).map_err(DbError::MigrationsDiffer)
}

/// The decision of [`require_current`] on rows of (version, checksum, success).
fn compare_migrations(
    migrator: &Migrator,
    applied: &[(i64, Vec<u8>, bool)],
) -> Result<(), MigrationsDiffer> {
    for (version, checksum, success) in applied {
        if !success {
            return Err(MigrationsDiffer::Dirty);
        }
        match up_migrations(migrator).find(|m| m.version == *version) {
            None => return Err(MigrationsDiffer::Ahead),
            Some(m) if m.checksum.as_ref() != checksum.as_slice() => {
                return Err(MigrationsDiffer::Changed);
            }
            Some(_) => {}
        }
    }
    if up_migrations(migrator).any(|m| !applied.iter().any(|(version, _, _)| *version == m.version))
    {
        return Err(MigrationsDiffer::Behind);
    }
    Ok(())
}

/// The migrations that are applied going up: sqlx's own run skips the down ones in the same way.
fn up_migrations(migrator: &Migrator) -> impl Iterator<Item = &sqlx::migrate::Migration> {
    migrator
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
}

/// A pool that connects on first use. Tests only: they exercise the routes without a schema.
#[doc(hidden)]
pub fn pool_lazy(options: sqlx::postgres::PgConnectOptions, timeout: Duration) -> PgPool {
    PgPoolOptions::new()
        .acquire_timeout(timeout)
        .connect_lazy_with(options)
}

/// Applies the embedded migrations, at most [`MIGRATION_TIMEOUT`].
pub async fn migrate(pool: &PgPool) -> Result<(), DbError> {
    migrate_with(pool, &MIGRATOR, MIGRATION_TIMEOUT).await
}

/// How much longer than the server-side limit the client waits before it gives up on its own.
const CLIENT_BACKSTOP: Duration = Duration::from_secs(5);

/// Applies `migrator`'s migrations within `timeout`. Tests only: the server calls [`migrate`].
///
/// sqlx holds an advisory lock for the run, which is what makes two servers starting at once
/// safe: one applies, the other waits and then finds nothing to do. The run uses one connection
/// of its own, on which `lock_timeout` and `statement_timeout` are set to `timeout`, so that
/// PostgreSQL itself ends a wait for the lock and a statement that runs too long; dropping the
/// client's future alone would leave the backend running, and holding the lock. The client waits
/// 5 s longer as a backstop. On any failure the connection is closed, which ends the session and
/// so releases the lock.
///
/// A failed run is never retried on the same pool, whichever way it failed: an advisory lock is
/// held per session and counts how often it was taken, so a retry on a session that still holds
/// the lock takes it again, and sqlx's single unlock then leaves it held. The caller closes the
/// pool and exits. A migration that was applied and has since changed, or one the binary does
/// not carry, is refused.
#[doc(hidden)]
pub async fn migrate_with(
    pool: &PgPool,
    migrator: &Migrator,
    timeout: Duration,
) -> Result<(), DbError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| DbError::Migrate(e.into()))?;
    let limit_ms = timeout.as_millis();
    for setting in ["lock_timeout", "statement_timeout"] {
        let sql = sqlx::AssertSqlSafe(format!("SET {setting} = {limit_ms}"));
        if let Err(error) = sqlx::query(sql).execute(&mut *conn).await {
            discard(conn).await;
            return Err(DbError::Migrate(error.into()));
        }
    }
    let run = tokio::time::timeout(
        timeout + CLIENT_BACKSTOP,
        migrator.run_direct(None, &mut *conn, false),
    )
    .await;
    let result = match run {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(DbError::Migrate(error)),
        Err(_) => Err(DbError::MigrateTimeout(timeout.as_secs())),
    };
    match result {
        Ok(()) => {
            // The connection goes back to the pool: the server's queries get no time limit.
            for setting in ["lock_timeout", "statement_timeout"] {
                let sql = sqlx::AssertSqlSafe(format!("RESET {setting}"));
                if let Err(error) = sqlx::query(sql).execute(&mut *conn).await {
                    discard(conn).await;
                    return Err(DbError::Migrate(error.into()));
                }
            }
            Ok(())
        }
        Err(error) => {
            discard(conn).await;
            Err(error)
        }
    }
}

/// Closes a pooled connection for good, ending its session on the server.
async fn discard(conn: sqlx::pool::PoolConnection<sqlx::Postgres>) {
    use sqlx::Connection;
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, conn.detach().close()).await;
}

/// Closes the pool, waiting for its connections no longer than a few seconds.
pub async fn close(pool: &PgPool) {
    if tokio::time::timeout(CLOSE_TIMEOUT, pool.close())
        .await
        .is_err()
    {
        tracing::warn!("the database pool did not close in time");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_migrations_are_the_files_of_the_directory() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../db/migrations");
        let mut files = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".sql"))
            .collect::<Vec<_>>();
        files.sort();
        let embedded = MIGRATOR.iter().collect::<Vec<_>>();
        assert_eq!(embedded.len(), files.len(), "{files:?}");
        assert!(!embedded.is_empty());
        for (m, f) in embedded.iter().zip(&files) {
            // Names are `NNNN_snake_case.sql`: the digits are the version.
            let (digits, rest) = f.split_once('_').expect("NNNN_name.sql");
            assert_eq!(m.version, digits.parse::<i64>().unwrap(), "{f}");
            assert!(digits.len() == 4 && rest.ends_with(".sql"), "{f}");
        }
    }

    #[test]
    fn the_read_only_check_reads_the_table_sqlx_writes() {
        assert_eq!(MIGRATOR.table_name, MIGRATIONS_TABLE);
        // `run_direct` fails on a recorded migration the binary lacks unless this is set, which
        // is the behaviour `compare_migrations` reproduces with its "ahead" refusal.
        assert!(!MIGRATOR.ignore_missing);
    }

    #[test]
    fn the_comparison_refuses_every_difference_and_accepts_equality() {
        let equal: Vec<_> = MIGRATOR
            .iter()
            .map(|m| (m.version, m.checksum.to_vec(), true))
            .collect();
        assert_eq!(compare_migrations(&MIGRATOR, &equal), Ok(()));

        let mut behind = equal.clone();
        behind.pop();
        assert_eq!(
            compare_migrations(&MIGRATOR, &behind),
            Err(MigrationsDiffer::Behind)
        );
        assert_eq!(
            compare_migrations(&MIGRATOR, &[]),
            Err(MigrationsDiffer::Behind)
        );

        let mut ahead = equal.clone();
        ahead.push((9999, vec![0], true));
        assert_eq!(
            compare_migrations(&MIGRATOR, &ahead),
            Err(MigrationsDiffer::Ahead)
        );

        let mut changed = equal.clone();
        changed[0].1 = vec![0];
        assert_eq!(
            compare_migrations(&MIGRATOR, &changed),
            Err(MigrationsDiffer::Changed)
        );

        let mut dirty = equal;
        dirty[1].2 = false;
        assert_eq!(
            compare_migrations(&MIGRATOR, &dirty),
            Err(MigrationsDiffer::Dirty)
        );
    }

    #[test]
    fn no_embedded_migration_contains_a_carriage_return() {
        // The checksum covers the bytes: a checkout that converts line endings would make an
        // applied migration look changed (`.gitattributes` pins LF).
        for m in MIGRATOR.iter() {
            assert!(!m.sql.as_str().contains('\r'), "migration {}", m.version);
        }
    }
}

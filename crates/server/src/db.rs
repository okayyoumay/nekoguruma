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
    fn no_embedded_migration_contains_a_carriage_return() {
        // The checksum covers the bytes: a checkout that converts line endings would make an
        // applied migration look changed (`.gitattributes` pins LF).
        for m in MIGRATOR.iter() {
            assert!(!m.sql.as_str().contains('\r'), "migration {}", m.version);
        }
    }
}

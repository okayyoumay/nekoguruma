//! The server's end of a job that was not started by its deadline (design 5.3, 8.8, 16.3;
//! ADR-487): one task of the hub's set that, every [`PERIOD`], ends the jobs still `received`
//! whose `start_deadline` the database's clock has reached as `expired`, and deletes the vehicle
//! lock each of them holds in the same statement.
//!
//! A pass is one statement in one transaction. It judges only the database's `now()`; the
//! process's clock paces the tick and nothing else. The rows are locked with `FOR UPDATE SKIP
//! LOCKED`, as the retired-key sweep's are ([`crate::job_queue`]): a row a delivery's take, an
//! answer or an event holds is left for the next pass, since waiting for it could deadlock with
//! the transaction that holds it and waits for another row. The job's state is the only thing the
//! statement judges: a job whose first report past `received` has not reached the server is
//! `received` here, and is ended while it runs (ADR-487 Consequences). A delivered job's
//! `delivered_at`, `last_reported_at` and mark stay as they are, and no failure code is written.
//!
//! The end writes no `agent_events` row (the server's end is not an agent's event, so the agent's
//! cursor does not move), no `change_feed` row and no `audit_log` row (ADR-487). A failed pass
//! writes nothing, is logged at most once in [`LOG_EVERY`] and is tried again at the next tick.
//! The task ends when the stop begins, in its wait and in the statement, before the pool closes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::db::{delete_own_lock, failure, statement_timeouts};
use crate::health::may_log;
use crate::session::now;

/// The time between two passes (the maintainer's decision, ADR-487).
pub const PERIOD: Duration = Duration::from_secs(30);
/// A failed pass is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);

/// Ends the jobs that are `received`, not finished and at or past their start deadline, skipping
/// rows another transaction holds; the result counts the ended jobs, as the delete's rows do not.
/// A job that names a VIN loses the lock row that names it (design 8.8); another job's row, or
/// none, stays.
#[doc(hidden)]
pub const EXPIRE_SQL: &str = concat!(
    "\
WITH ended AS ( \
  UPDATE jobs SET state = 'expired', finished_at = now() \
  WHERE (tenant_id, id) IN ( \
    SELECT tenant_id, id FROM jobs \
    WHERE state = 'received' AND finished_at IS NULL AND start_deadline <= now() \
    FOR UPDATE SKIP LOCKED) \
  RETURNING tenant_id, id, vin), \
released AS ( ",
    delete_own_lock!(),
    ") \
SELECT count(*) FROM ended"
);

/// When the task passes. Tests shorten the period and count the passes.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct Settings {
    /// The time between two passes.
    pub period: Duration,
    /// Counts the passes that committed (a test waits on it).
    pub passes: Option<Arc<AtomicU64>>,
    /// Counts the passes that failed (a test waits on it).
    pub failures: Option<Arc<AtomicU64>>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            period: PERIOD,
            passes: None,
            failures: None,
        }
    }
}

/// One pass: the statement in a transaction with the 5-second timeouts, committed. Returns how
/// many jobs it ended.
#[doc(hidden)]
pub async fn expire(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    statement_timeouts(&mut tx).await?;
    let (ended,): (i64,) = sqlx::query_as(EXPIRE_SQL).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(u64::try_from(ended).unwrap_or(0))
}

/// Resolves when the stop has begun (or the hub is gone).
async fn stopped(stopping: &mut watch::Receiver<bool>) {
    let _ = stopping.wait_for(|stop| *stop).await;
}

/// Runs the passes until the stop. A stop during the statement drops it, which rolls the
/// transaction back.
pub(crate) async fn run(pool: PgPool, mut stopping: watch::Receiver<bool>, settings: Settings) {
    // A tick missed because the task was slow gives one pass, not a burst.
    let mut ticks = interval_at(Instant::now() + settings.period, settings.period);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let gate = Mutex::new(None);
    loop {
        tokio::select! {
            () = stopped(&mut stopping) => return,
            _ = ticks.tick() => {}
        }
        let result = tokio::select! {
            () = stopped(&mut stopping) => return,
            result = expire(&pool) => result,
        };
        match result {
            Ok(ended) => {
                if ended > 0 {
                    tracing::info!(jobs = ended, "jobs past their start deadline were expired");
                }
                if let Some(passes) = &settings.passes {
                    passes.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(error) => {
                if may_log(&gate, now(), LOG_EVERY) {
                    let what = failure(&error);
                    tracing::warn!(
                        ?what,
                        "the job expiry pass failed; it is tried again at the next tick"
                    );
                }
                if let Some(failures) = &settings.failures {
                    failures.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

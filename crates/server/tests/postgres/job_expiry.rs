//! The server's end of a job past its start deadline against a throwaway PostgreSQL (design 5.3,
//! 8.8, 16.3; ADR-487): the statement ([`server::job_expiry::EXPIRE_SQL`], run by `expire`) and the
//! task that runs it. The case names in the comments are the rows of ADR-487's case table.
//!
//! The jobs are rows the test inserts (`job_queue::add_job_with` with a negative deadline is a
//! deadline in the past). The tests run on the current-thread runtime and wait in real time.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use server::Server;
use server::config::{Config, Database, Http, InstructionKeyConfig, Profile, StorageConfig};
use server::job_expiry::{EXPIRE_SQL, Settings, expire};
use server::job_queue::TAKE_SQL;

use crate::control::{open, token};
use crate::job_queue::{
    LONG, SENT, ack, add_job, add_job_with, command, reject, settings, until_logged, until_row,
};
use crate::redeem::{capture, instruction_key};
use crate::serving::Running;
use crate::session::Up;
use crate::support::TestPostgres;

const PERIOD: Duration = Duration::from_millis(50);
const VIN_A: &str = "1HGCM82633A004352";
const VIN_B: &str = "WVWZZZ1JZXW000001";
const VIN_C: &str = "JH4KA8260MC000000";
/// The delivery columns of a job the queue sent once and the expiry then ended.
const EXPIRED: &str = "expired|1|true|false|||true";
/// The same for a job that was never sent.
const ENDED: &str = "expired|0|false|false|||true";

fn expiry(passes: &Arc<AtomicU64>, failures: &Arc<AtomicU64>) -> Settings {
    Settings {
        period: PERIOD,
        passes: Some(passes.clone()),
        failures: Some(failures.clone()),
    }
}

/// A server with no queue and no task, and a registered agent: the statement is run by hand.
async fn quiet() -> Up {
    Up::new().await
}

async fn run(u: &Up, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
        .execute(&u.w.pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// A received job of the agent whose deadline is `secs` from now (negative: past).
async fn job(u: &Up, secs: i64) -> String {
    add_job_with(&u.w, &u.id, 0, b"x", &u.w.key.key_id(), secs).await
}

async fn set(u: &Up, job: &str, assignments: &str) {
    run(
        u,
        &format!("UPDATE jobs SET {assignments} WHERE id = '{job}'"),
    )
    .await;
}

/// The whole row as text, so that any change to it shows.
async fn snapshot(u: &Up, job: &str) -> String {
    u.w.sql(&format!(
        "SELECT row_to_json(j)::text FROM jobs j WHERE id = '{job}'"
    ))
    .await
}

/// The columns an end touches, as one line: state, whether finished, failure code and detail
/// (`-` for none), whether delivered, marked and reported on.
async fn line(u: &Up, job: &str) -> String {
    u.w.sql(&format!(
        "SELECT concat_ws('|', state, (finished_at IS NOT NULL)::text, coalesce(failure_code, '-'), \
             coalesce(failure_detail, '-'), (delivered_at IS NOT NULL)::text, \
             (delivery_sent_at IS NOT NULL)::text, (last_reported_at IS NOT NULL)::text) \
         FROM jobs WHERE id = '{job}'"
    ))
    .await
}

/// Makes the job the holder of the VIN's lock for an hour.
async fn hold(u: &Up, job: &str, vin: &str) {
    set(u, job, &format!("vin = '{vin}'")).await;
    run(
        u,
        &format!(
            "INSERT INTO vehicle_locks (tenant_id, vin, job_id, agent_id, expires_at) \
             SELECT tenant_id, vin, id, agent_id, now() + interval '1 hour' FROM jobs \
             WHERE id = '{job}'"
        ),
    )
    .await;
}

async fn holders(u: &Up) -> String {
    u.w.sql(
        "SELECT coalesce(string_agg(vin || '>' || job_id::text, ',' ORDER BY vin), '') \
         FROM vehicle_locks",
    )
    .await
}

/// Counts of the tables an end must not write to (E4, E5, E6).
async fn other_writes(u: &Up) -> String {
    u.w.sql(
        "SELECT concat_ws('|', (SELECT count(*) FROM agent_events), \
             (SELECT count(*) FROM agent_event_cursor), (SELECT count(*) FROM change_feed), \
             (SELECT count(*) FROM audit_log))",
    )
    .await
}

async fn until_at_least(counter: &AtomicU64, at_least: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while counter.load(Ordering::Relaxed) < at_least {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// X1, X2 (past), X3, X5, X6, E4, E5, E6: from its deadline a received job is ended, whether or
// not it was delivered; nothing before it; no failure; no other table is written.
#[tokio::test]
async fn a_received_job_is_ended_as_expired_from_its_deadline_and_not_before() {
    let u = quiet().await;
    let before_undelivered = job(&u, 3600).await;
    let before_delivered = job(&u, 3600).await;
    set(
        &u,
        &before_delivered,
        "delivered_at = now(), last_reported_at = now()",
    )
    .await;
    let past_undelivered = job(&u, -1).await;
    let past_delivered = job(&u, -1).await;
    set(
        &u,
        &past_delivered,
        "delivered_at = now(), last_reported_at = now()",
    )
    .await;
    // X4's row state: a live mark.
    set(
        &u,
        &past_undelivered,
        "delivery_sent_at = now(), delivery_attempts = 1",
    )
    .await;
    let untouched = (
        snapshot(&u, &before_undelivered).await,
        snapshot(&u, &before_delivered).await,
    );
    let writes = other_writes(&u).await;

    assert_eq!(expire(&u.w.pool).await.unwrap(), 2);

    // X3 and X4: expired, ended now, no failure, the mark kept.
    assert_eq!(
        line(&u, &past_undelivered).await,
        "expired|true|-|-|false|true|false"
    );
    // X5: delivered_at and last_reported_at are kept.
    assert_eq!(
        line(&u, &past_delivered).await,
        "expired|true|-|-|true|false|true"
    );
    // X1, X6: untouched.
    assert_eq!(snapshot(&u, &before_undelivered).await, untouched.0);
    assert_eq!(snapshot(&u, &before_delivered).await, untouched.1);
    assert_eq!(
        u.w.sql(&format!(
            "SELECT (finished_at <= now() AND finished_at > now() - interval '1 minute')::text \
             FROM jobs WHERE id = '{past_undelivered}'"
        ))
        .await,
        "true"
    );
    // E4, E5, E6.
    assert_eq!(other_writes(&u).await, writes);
    u.finish().await;
}

/// The rows the delivery queue's take returns for the agent in this transaction.
async fn take(tx: &mut sqlx::PgConnection, agent: &str) -> Vec<sqlx::postgres::PgRow> {
    sqlx::query(TAKE_SQL)
        .bind(vec![agent.to_owned()])
        .bind(90.0_f64)
        .bind(Vec::<String>::new())
        .bind(8_i64)
        .bind(64_i64)
        .fetch_all(tx)
        .await
        .unwrap()
}

// X2, R2: at the very instant of the deadline the take leaves the job and the statement ends it
// (one transaction has one `now()`), and after the end the take leaves it for good.
#[tokio::test]
async fn at_its_deadline_to_the_instant_the_take_refuses_the_job_and_the_expiry_ends_it() {
    let u = quiet().await;
    let job = job(&u, 3600).await;
    let mut tx = u.w.pool.begin().await.unwrap();
    sqlx::query("UPDATE jobs SET start_deadline = now() WHERE id = $1::uuid")
        .bind(&job)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(take(&mut tx, &u.id).await.is_empty(), "the take refuses it");
    let (ended,): (i64,) = sqlx::query_as(EXPIRE_SQL)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(ended, 1, "the expiry takes it at the same instant");
    tx.commit().await.unwrap();
    assert_eq!(line(&u, &job).await, "expired|true|-|-|false|false|false");
    u.finish().await;
}

// X7, X8, X9, X10: only a job still `received` and not finished is touched; a second pass
// rewrites nothing.
#[tokio::test]
async fn no_other_state_and_no_finished_job_is_touched_and_a_second_pass_rewrites_nothing() {
    let u = quiet().await;
    let mut untouched = Vec::new();
    // X7, X9: every open state past its deadline, with and without delivered_at. A job that an
    // agent's recovery end was reopened by a report is an open state with no end.
    for state in [
        "precheck",
        "ready",
        "running",
        "verifying",
        "awaitingHmi",
        "cancelRequested",
    ] {
        for delivered in ["NULL", "now()"] {
            let id = job(&u, -60).await;
            set(
                &u,
                &id,
                &format!("state = '{state}', delivered_at = {delivered}"),
            )
            .await;
            untouched.push(id);
        }
    }
    // X8: every finished state, X10: received with an end.
    for state in [
        "completed",
        "failed",
        "cancelled",
        "expired",
        "preconditionFailed",
        "needsOnSiteRecovery",
        "received",
    ] {
        let id = job(&u, -60).await;
        set(
            &u,
            &id,
            &format!("state = '{state}', finished_at = now() - interval '5 minutes'"),
        )
        .await;
        untouched.push(id);
    }
    let mut before = Vec::new();
    for id in &untouched {
        before.push(snapshot(&u, id).await);
    }
    assert_eq!(expire(&u.w.pool).await.unwrap(), 0);
    for (id, was) in untouched.iter().zip(&before) {
        assert_eq!(&snapshot(&u, id).await, was, "{id}");
    }
    // A second pass after a real end rewrites nothing: the end keeps its time.
    let ended = job(&u, -1).await;
    assert_eq!(expire(&u.w.pool).await.unwrap(), 1);
    let first = snapshot(&u, &ended).await;
    assert_eq!(expire(&u.w.pool).await.unwrap(), 0);
    assert_eq!(snapshot(&u, &ended).await, first);
    u.finish().await;
}

// L1 to L4: the job's own lock goes with it, and no other row does.
#[tokio::test]
async fn the_lock_row_that_names_the_ended_job_goes_with_it_and_no_other_row_does() {
    let u = quiet().await;
    // L1
    let holder = job(&u, -1).await;
    hold(&u, &holder, VIN_A).await;
    // L2: no VIN; L4: a VIN with no row.
    let no_vin = job(&u, -1).await;
    let no_row = job(&u, -1).await;
    set(&u, &no_row, &format!("vin = '{VIN_C}'")).await;
    // L3: the row names another job, which is live and keeps it.
    let live = job(&u, 3600).await;
    let mismatch = job(&u, -1).await;
    hold(&u, &live, VIN_B).await;
    set(&u, &mismatch, &format!("vin = '{VIN_B}'")).await;
    // L4: the same VIN locked in another tenant by a job of that tenant.
    let other_tenant = job(&u, -1).await;
    set(&u, &other_tenant, &format!("vin = '{VIN_C}'")).await;
    run(
        &u,
        &format!(
            "WITH t AS (INSERT INTO tenants (id, name) VALUES (gen_random_uuid(), 'other') \
                            RETURNING id) \
             INSERT INTO vehicle_locks (tenant_id, vin, job_id, agent_id, expires_at) \
             SELECT t.id, '{VIN_C}', gen_random_uuid(), '{}', now() + interval '1 hour' FROM t",
            u.id
        ),
    )
    .await;
    let foreign =
        u.w.sql(&format!(
            "SELECT job_id::text FROM vehicle_locks WHERE vin = '{VIN_C}'"
        ))
        .await;

    assert_eq!(expire(&u.w.pool).await.unwrap(), 5);
    for ended in [&holder, &no_vin, &no_row, &mismatch, &other_tenant] {
        assert_eq!(line(&u, ended).await, "expired|true|-|-|false|false|false");
    }
    assert_eq!(
        line(&u, &live).await,
        "received|false|-|-|false|false|false"
    );
    // Of three rows only the holder's own went: the live job's and the other tenant's stay.
    assert_eq!(
        holders(&u).await,
        format!("{VIN_C}>{foreign},{VIN_B}>{live}")
    );
    u.finish().await;
}

// R14, W7: the end and the lock's deletion are one statement: a deletion that fails leaves the
// job received, and the lock where it was.
#[tokio::test]
async fn a_lock_that_cannot_be_deleted_leaves_the_job_unended_because_it_is_one_statement() {
    let u = quiet().await;
    let job = job(&u, -1).await;
    hold(&u, &job, VIN_A).await;
    let before = snapshot(&u, &job).await;
    run(
        &u,
        "CREATE FUNCTION refuse_delete() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'refused'; END $$; \
         CREATE TRIGGER refuse BEFORE DELETE ON vehicle_locks \
         FOR EACH ROW EXECUTE FUNCTION refuse_delete()",
    )
    .await;
    assert!(expire(&u.w.pool).await.is_err());
    assert_eq!(snapshot(&u, &job).await, before);
    assert_eq!(holders(&u).await, format!("{VIN_A}>{job}"));
    run(&u, "DROP TRIGGER refuse ON vehicle_locks").await;
    assert_eq!(expire(&u.w.pool).await.unwrap(), 1);
    assert_eq!(holders(&u).await, "");
    u.finish().await;
}

// R1, R3, R5, R6: a row another transaction holds is skipped, not waited for, and ended by the
// next pass.
#[tokio::test]
async fn a_row_another_transaction_holds_is_skipped_at_once_and_ended_by_the_next_pass() {
    let u = quiet().await;
    let held = job(&u, -1).await;
    let free = job(&u, -1).await;
    let mut tx = u.w.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1::uuid FOR UPDATE")
        .bind(&held)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    let started = Instant::now();
    assert_eq!(expire(&u.w.pool).await.unwrap(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "it did not wait"
    );
    assert_eq!(
        line(&u, &held).await,
        "received|false|-|-|false|false|false"
    );
    assert_eq!(line(&u, &free).await, "expired|true|-|-|false|false|false");
    tx.commit().await.unwrap();
    assert_eq!(expire(&u.w.pool).await.unwrap(), 1);
    assert_eq!(line(&u, &held).await, "expired|true|-|-|false|false|false");
    u.finish().await;
}

// R12: two servers passing at once end each job once, and the counts sum to the total.
#[tokio::test]
async fn two_passes_at_once_end_each_job_once_and_free_each_lock_once() {
    let u = quiet().await;
    for n in 0..40 {
        let job = job(&u, -1).await;
        hold(&u, &job, &format!("1HGCM82633A{n:06}")).await;
    }
    let (a, b) = (u.w.pg.pool(), u.w.pg.pool());
    let (first, second) = tokio::join!(expire(&a), expire(&b));
    assert_eq!(first.unwrap() + second.unwrap(), 40);
    assert_eq!(
        u.w.sql("SELECT count(*) FROM jobs WHERE state = 'expired'")
            .await,
        "40"
    );
    assert_eq!(u.w.sql("SELECT count(*) FROM vehicle_locks").await, "0");
    u.finish().await;
}

// R16: a revoked agent's undelivered job is ended and its vehicle freed like any other.
#[tokio::test]
async fn a_revoked_agents_undelivered_job_is_ended_and_its_lock_freed() {
    let u = quiet().await;
    let job = job(&u, -1).await;
    hold(&u, &job, VIN_A).await;
    run(&u, "UPDATE agents SET revoked_at = now()").await;
    assert_eq!(expire(&u.w.pool).await.unwrap(), 1);
    assert_eq!(line(&u, &job).await, "expired|true|-|-|false|false|false");
    assert_eq!(holders(&u).await, "");
    u.finish().await;
}

// X4, R8: after the end, an agent's acknowledgement, its rejection and the departure of its
// connection write nothing; the row is as the end left it.
#[tokio::test]
async fn an_answer_or_a_departure_after_the_end_writes_nothing_to_the_job() {
    let u = Up::with_queue(2, Some(settings(LONG))).await;
    let acked = add_job(&u.w, &u.id, 1, b"a").await;
    let rejected = add_job(&u.w, &u.id, 2, b"b").await;
    let left = add_job(&u.w, &u.id, 3, b"c").await;
    let mut client = open(u.s.addr, &token(&u).await).await;
    let (id_a, _) = command(&mut client).await;
    let (id_b, _) = command(&mut client).await;
    command(&mut client).await;
    for job in [&acked, &rejected, &left] {
        until_row(&u.w, job, SENT).await;
    }
    run(
        &u,
        "UPDATE jobs SET start_deadline = now() - interval '1 second'",
    )
    .await;
    assert_eq!(expire(&u.w.pool).await.unwrap(), 3);
    ack(&mut client, &id_a).await;
    reject(&mut client, &id_b, "badSignature").await;
    drop(client);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !u.hub.connected().is_empty() {
        assert!(Instant::now() < deadline, "the connection did not leave");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Something positive that comes later in the same order: the queue delivers a new job to a
    // new connection, which it does after it has written the answers and the release.
    let next = add_job(&u.w, &u.id, 4, b"d").await;
    let mut again = open(u.s.addr, &token(&u).await).await;
    assert_eq!(command(&mut again).await.1, b"d");
    until_row(&u.w, &next, SENT).await;
    for job in [&acked, &rejected, &left] {
        assert_eq!(crate::job_queue::row(&u.w, job).await, EXPIRED, "{job}");
    }
    u.finish().await;
}

// R13, R17: the task ends jobs that were past their deadline when it started and leaves the
// others; a stop during its statement ends it before the pool closes, and nothing runs after.
#[tokio::test]
async fn the_task_ends_what_is_past_at_its_start_and_a_stop_ends_it_during_its_statement() {
    let passes = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(AtomicU64::new(0));
    // A grace above the statement's 5 s lock timeout: a task that ignored the stop would then
    // see its statement time out (a failure) before the hub aborted it.
    let u = Up::with_tasks(8, None, Some(expiry(&passes, &failures))).await;
    let Up { w, s, id, .. } = u;
    let past = add_job_with(&w, &id, 0, b"x", &w.key.key_id(), -1).await;
    let future = add_job_with(&w, &id, 1, b"x", &w.key.key_id(), 3600).await;
    until_row(&w, &past, ENDED).await;
    assert_eq!(
        crate::job_queue::row(&w, &future).await,
        "received|0|false|false|||false"
    );
    // A statement that waits on a lock for the whole 5 s timeout: the stop must not wait for it.
    let mut tx = w.pool.begin().await.unwrap();
    sqlx::raw_sql("LOCK TABLE jobs IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    let seen = passes.load(Ordering::Relaxed);
    tokio::time::sleep(PERIOD * 4).await;
    assert_eq!(
        passes.load(Ordering::Relaxed),
        seen,
        "the pass is waiting on the lock"
    );
    // The pool closes only after the hub's tasks have ended, so nothing of the task is left to
    // run against a closed pool (the statement's own wait on the database is bounded by its 5 s
    // timeout, which the shutdown may still wait for).
    s.finish().await;
    tx.rollback().await.unwrap();
    // No pass runs after the stop, and the one it cut off is not counted as a failure.
    let after = passes.load(Ordering::Relaxed);
    tokio::time::sleep(PERIOD * 6).await;
    assert_eq!(passes.load(Ordering::Relaxed), after);
    assert_eq!(failures.load(Ordering::Relaxed), 0);
    w.finish().await;
}

// R14: a failed pass writes nothing, is logged once in a minute however often it fails, and the
// task goes on.
#[tokio::test]
async fn a_failing_pass_is_logged_once_and_retried_at_every_tick_until_it_works() {
    let (log, _guard) = capture();
    let passes = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(AtomicU64::new(0));
    let u = Up::with_tasks(2, None, Some(expiry(&passes, &failures))).await;
    run(
        &u,
        "CREATE FUNCTION refuse_delete() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'refused'; END $$; \
         CREATE TRIGGER refuse BEFORE DELETE ON vehicle_locks \
         FOR EACH ROW EXECUTE FUNCTION refuse_delete()",
    )
    .await;
    let job = job(&u, -1).await;
    hold(&u, &job, VIN_A).await;
    until_at_least(&failures, 3, "the pass failed three times").await;
    until_logged(&log, "the job expiry pass failed").await;
    assert_eq!(log.text().matches("the job expiry pass failed").count(), 1);
    assert_eq!(line(&u, &job).await, "received|false|-|-|false|false|false");
    assert_eq!(holders(&u).await, format!("{VIN_A}>{job}"));
    run(&u, "DROP TRIGGER refuse ON vehicle_locks").await;
    until_row(&u.w, &job, ENDED).await;
    assert_eq!(holders(&u).await, "");
    u.finish().await;
}

/// `Server::start` turns the task on, `Server::bind` leaves it off.
#[tokio::test]
async fn start_turns_the_expiry_task_on_and_bind_leaves_it_off() {
    let pg = TestPostgres::start();
    let mut database = Database::new(pg.url());
    database.connect_timeout_secs = 5;
    let config = Config {
        profile: Profile::Local,
        http: Http {
            port: 0,
            shutdown_grace_secs: 2,
        },
        database,
        instruction_key: InstructionKeyConfig {
            path: pg.key_path(),
        },
        storage: StorageConfig::new(pg.storage_path()),
    };
    let started = Server::start(&config).await.unwrap();
    assert!(started.job_expiry_enabled());
    Running::spawn(started).finish().await;
    let bound = Server::bind(&config.http, pg.pool(), instruction_key())
        .await
        .unwrap();
    assert!(!bound.job_expiry_enabled());
}

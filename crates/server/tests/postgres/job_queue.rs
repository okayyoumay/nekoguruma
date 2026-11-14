//! The job delivery queue against a throwaway PostgreSQL (design 5.3, 14.1, ADR-476): fake agents
//! are WebSocket clients of a server whose queue is on with a short period, and the jobs are rows
//! the test inserts. The steps that need no database are unit tests in `src/job_queue.rs`.
//!
//! The tests run on the current-thread runtime and wait in real time, never on a paused clock
//! (ADR-467): a delivery needs a pass, which needs the database. A negative (a frame that must not
//! come, a row that must not change) is asserted only after something positive that comes later
//! in the same order has been seen.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::SinkExt;
use server::Server;
use server::config::{Config, Database, Http, InstructionKeyConfig, Profile, StorageConfig};
use server::job_queue::{Settings, TAKE_SQL};
use shared_proto::ServerMessage;
use tokio_tungstenite::tungstenite::Message;

use crate::control::{Client, close_code, heartbeat, open, progress, quiet_for, revoke_row};
use crate::control::{second_agent, text, token};
use crate::redeem::{World, capture, instruction_key, json};
use crate::serving::Running;
use crate::session::Up;
use crate::support::TestPostgres;

const PERIOD: Duration = Duration::from_millis(50);
/// A lease no test waits for.
const LONG: Duration = Duration::from_secs(600);
const FRESH: &str = "received|0|false|false|||false";
const SENT: &str = "received|1|true|false|||false";
const DELIVERED: &str = "received|1|true|true|||false";
const RETIRED: &str = "failed|0|false|false|instructionKeyRetired|the instruction was signed by a key that is no longer active|true";

fn settings(lease: Duration) -> Settings {
    Settings {
        period: PERIOD,
        lease,
        passes: None,
    }
}

async fn up(lease: Duration) -> Up {
    Up::with_queue(2, Some(settings(lease))).await
}

/// A job row for `agent`, created `order` seconds after an arbitrary earlier moment, so that a
/// smaller `order` is delivered first; its instruction is the bytes `bytes`.
async fn add_job_with(
    w: &World,
    agent: &str,
    order: i64,
    bytes: &[u8],
    key_id: &str,
    deadline_secs: i64,
) -> String {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, id, kind, agent_id, operator_id, vci_id, \
             confirmation_level, start_deadline, signed_job, signing_key_id, created_at, \
             idempotency_key) \
         SELECT a.tenant_id, gen_random_uuid(), 'runSequence', a.id, o.id, 'vci', 'notifyOnly', \
             now() + $3 * interval '1 second', $4, $5, \
             now() - interval '1 hour' + $2 * interval '1 second', gen_random_uuid()::text \
         FROM agents a JOIN operators o ON o.tenant_id = a.tenant_id WHERE a.id = $1::uuid \
         RETURNING id::text",
    )
    .bind(agent)
    .bind(order)
    .bind(deadline_secs)
    .bind(bytes)
    .bind(key_id)
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

async fn add_job(w: &World, agent: &str, order: i64, bytes: &[u8]) -> String {
    add_job_with(w, agent, order, bytes, &w.key.key_id(), 3600).await
}

/// The delivery columns and the end of the job, as one line.
async fn row(w: &World, job: &str) -> String {
    sqlx::query_scalar(
        "SELECT concat_ws('|', state, delivery_attempts, \
             (delivery_sent_at IS NOT NULL)::text, (delivered_at IS NOT NULL)::text, \
             coalesce(failure_code, ''), coalesce(failure_detail, ''), \
             (finished_at IS NOT NULL)::text) FROM jobs WHERE id = $1::uuid",
    )
    .bind(job)
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

async fn until_row(w: &World, job: &str, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let now = row(w, job).await;
        if now == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the row is {now}, not {expected}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The next job command: its command ID and the stored bytes it carries.
async fn command(client: &mut Client) -> (String, Vec<u8>) {
    match ServerMessage::decode(&text(client).await).unwrap() {
        ServerMessage::JobCommand {
            command_id,
            signed_job,
        } => (command_id, STANDARD.decode(signed_job.unwrap()).unwrap()),
        other => panic!("a job command was due: {other:?}"),
    }
}

async fn answer(client: &mut Client, body: String) {
    client.send(Message::Text(body.into())).await.unwrap();
}

async fn ack(client: &mut Client, command_id: &str) {
    answer(
        client,
        format!(r#"{{"type":"commandAck","commandId":"{command_id}"}}"#),
    )
    .await;
}

async fn reject(client: &mut Client, command_id: &str, reason: &str) {
    answer(
        client,
        format!(r#"{{"type":"commandRejected","commandId":"{command_id}","reason":"{reason}"}}"#),
    )
    .await;
}

/// Another server on the same database, with its own hub and pool, and its queue on.
async fn second_server(u: &Up, lease: Duration) -> Running {
    let http = Http {
        port: 0,
        shutdown_grace_secs: 2,
    };
    let server = Server::bind(&http, u.w.pg.pool(), u.w.key.clone())
        .await
        .unwrap()
        .with_job_queue(settings(lease));
    Running::spawn(server)
}

async fn session_token(reply: crate::serving::Reply) -> String {
    assert_eq!(reply.status, 200, "{}", reply.head);
    json(&reply)["token"].as_str().unwrap().to_owned()
}

// Case 1
#[tokio::test]
async fn a_queued_job_is_sent_once_with_its_stored_bytes_and_an_ack_settles_it() {
    let u = up(LONG).await;
    let a = add_job(&u.w, &u.id, 1, b"envelope-a").await;
    let mut client = open(u.s.addr, &token(&u).await).await;
    let (id, bytes) = command(&mut client).await;
    assert_eq!(bytes, b"envelope-a");
    until_row(&u.w, &a, SENT).await;
    ack(&mut client, &id).await;
    until_row(&u.w, &a, DELIVERED).await;
    // A job queued later is the next frame: the settled one was not sent again.
    let b = add_job(&u.w, &u.id, 2, b"envelope-b").await;
    assert_eq!(command(&mut client).await.1, b"envelope-b");
    until_row(&u.w, &b, SENT).await;
    assert_eq!(row(&u.w, &a).await, DELIVERED);
    u.finish().await;
}

// Case 2
#[tokio::test]
async fn a_job_of_an_agent_that_is_not_connected_is_not_taken_and_is_sent_on_connect() {
    let u = up(LONG).await;
    let (other_id, other_token) = second_agent(&u).await;
    let offline = add_job(&u.w, &u.id, 1, b"offline").await;
    let online = add_job(&u.w, &other_id, 2, b"online").await;
    let mut other = open(u.s.addr, &other_token).await;
    assert_eq!(command(&mut other).await.1, b"online");
    until_row(&u.w, &online, SENT).await;
    assert_eq!(row(&u.w, &offline).await, FRESH);
    let mut client = open(u.s.addr, &token(&u).await).await;
    assert_eq!(command(&mut client).await.1, b"offline");
    u.finish().await;
}

// Case 6
#[tokio::test]
async fn two_servers_on_one_database_send_every_job_on_exactly_one_socket() {
    let u = up(LONG).await;
    let s2 = second_server(&u, LONG).await;
    let (t1, t2) = (token(&u).await, session_token(u.valid_on(&s2).await).await);
    // The agent is connected to both servers before the jobs exist, so both passes see each row.
    let mut c1 = open(u.s.addr, &t1).await;
    let mut c2 = open(s2.addr, &t2).await;
    let jobs = [
        add_job(&u.w, &u.id, 1, b"j1").await,
        add_job(&u.w, &u.id, 2, b"j2").await,
        add_job(&u.w, &u.id, 3, b"j3").await,
        add_job(&u.w, &u.id, 4, b"j4").await,
        add_job(&u.w, &u.id, 5, b"j5").await,
    ];
    let mut got: Vec<Vec<u8>> = Vec::new();
    while got.len() < jobs.len() {
        let (on_first, (id, bytes)) = tokio::select! {
            r = command(&mut c1) => (true, r),
            r = command(&mut c2) => (false, r),
        };
        got.push(bytes);
        ack(if on_first { &mut c1 } else { &mut c2 }, &id).await;
    }
    for job in &jobs {
        until_row(&u.w, job, DELIVERED).await;
    }
    got.sort();
    assert_eq!(got, [b"j1", b"j2", b"j3", b"j4", b"j5"]);
    // All five are settled, so a sixth frame on either socket would be a duplicate.
    quiet_for(&mut c1, 300).await;
    quiet_for(&mut c2, 300).await;
    s2.finish().await;
    u.finish().await;
}

// Case 6b, and the EXPLAIN check
#[tokio::test]
async fn a_take_skips_the_rows_another_take_holds_and_a_marked_row_is_not_taken_again() {
    let u = Up::new().await;
    for n in 0..8 {
        add_job(&u.w, &u.id, n, b"x").await;
    }
    type Row = (String, String, String, Vec<u8>, i32, Option<String>);
    let take = |limit: i64| {
        sqlx::query_as::<_, Row>(TAKE_SQL)
            .bind(vec![u.id.clone()])
            .bind(60.0_f64)
            .bind(Vec::<String>::new())
            .bind(limit)
            .bind(1000_i64)
    };
    let mark =
        "UPDATE jobs SET delivery_attempts = 1, delivery_sent_at = now() WHERE id = $1::uuid";
    let mut t1 = u.w.pool.begin().await.unwrap();
    let first = take(5).fetch_all(&mut *t1).await.unwrap();
    let mut t2 = u.w.pool.begin().await.unwrap();
    let second = take(100).fetch_all(&mut *t2).await.unwrap();
    assert_eq!((first.len(), second.len()), (5, 3));
    assert!(second.iter().all(|s| first.iter().all(|f| f.1 != s.1)));
    for row in first.iter().chain(&second) {
        let tx = if first.contains(row) {
            &mut t1
        } else {
            &mut t2
        };
        sqlx::query(mark)
            .bind(&row.1)
            .execute(&mut **tx)
            .await
            .unwrap();
    }
    t1.commit().await.unwrap();
    t2.commit().await.unwrap();
    assert!(take(100).fetch_all(&u.w.pool).await.unwrap().is_empty());

    // The take reaches the rows through the partial index of the queue, also beside many jobs that
    // are open for another reason.
    sqlx::query(
        "INSERT INTO jobs (tenant_id, id, kind, agent_id, operator_id, vci_id, \
             confirmation_level, start_deadline, signed_job, signing_key_id, state, \
             idempotency_key) \
         SELECT a.tenant_id, gen_random_uuid(), 'runSequence', a.id, o.id, 'vci', 'notifyOnly', \
             now() + interval '1 hour', 'x', 'k', 'running', gen_random_uuid()::text \
         FROM agents a JOIN operators o ON o.tenant_id = a.tenant_id, generate_series(1, 5000) \
         WHERE a.id = $1::uuid",
    )
    .bind(&u.id)
    .execute(&u.w.pool)
    .await
    .unwrap();
    sqlx::query("ANALYZE jobs")
        .execute(&u.w.pool)
        .await
        .unwrap();
    let mut tx = u.w.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await
        .unwrap();
    let plan: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN {TAKE_SQL}")))
        .bind(vec![u.id.clone()])
        .bind(60.0_f64)
        .bind(Vec::<String>::new())
        .bind(10_i64)
        .bind(64_i64)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    let plan: String = plan.into_iter().map(|(l,)| l + "\n").collect();
    assert!(plan.contains("jobs_pending"), "{plan}");
    drop(tx);
    u.finish().await;
}

// Case 7
#[tokio::test]
async fn a_take_that_is_dropped_before_the_send_marks_nothing_and_the_next_pass_delivers_once() {
    let u = up(LONG).await;
    let held = add_job(&u.w, &u.id, 1, b"held").await;
    let free = add_job(&u.w, &u.id, 2, b"free").await;
    let mut tx = u.w.pool.begin().await.unwrap();
    let taken: Vec<Taken> = sqlx::query_as(TAKE_SQL)
        .bind(vec![u.id.clone()])
        .bind(60.0_f64)
        .bind(vec![free.clone()])
        .bind(1_i64)
        .bind(64_i64)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(taken[0].1, held);
    let mut client = open(u.s.addr, &token(&u).await).await;
    // The queue skips the locked row, and the later job shows that its passes are running.
    assert_eq!(command(&mut client).await.1, b"free");
    until_row(&u.w, &free, SENT).await;
    assert_eq!(row(&u.w, &held).await, FRESH);
    drop(tx);
    assert_eq!(command(&mut client).await.1, b"held");
    until_row(&u.w, &held, SENT).await;
    u.finish().await;
}

// Case 9
#[tokio::test]
async fn an_unanswered_delivery_outlives_its_server_and_is_resent_after_the_lease() {
    let lease = Duration::from_secs(2);
    let u = up(lease).await;
    let s2 = second_server(&u, lease).await;
    let (t1, t2) = (token(&u).await, session_token(u.valid_on(&s2).await).await);
    let job = add_job(&u.w, &u.id, 1, b"survivor").await;
    let mut c1 = open(u.s.addr, &t1).await;
    assert_eq!(command(&mut c1).await.1, b"survivor");
    until_row(&u.w, &job, SENT).await;
    let marked: String =
        sqlx::query_scalar("SELECT delivery_sent_at::text FROM jobs WHERE id = $1::uuid")
            .bind(&job)
            .fetch_one(&u.w.pool)
            .await
            .unwrap();
    // The first server stops without an answer; its stop releases nothing. The first socket is
    // read, so that it answers the Close and the stop is quick, well inside the lease.
    let Up { w, s, .. } = u;
    let (_, ()) = tokio::join!(close_code(&mut c1), s.finish());
    assert_eq!(row(&w, &job).await, SENT);
    let mut c2 = open(s2.addr, &t2).await;
    assert_eq!(command(&mut c2).await.1, b"survivor");
    let waited: bool =
        sqlx::query_scalar("SELECT now() - $1::timestamptz >= $2::float8 * interval '1 second'")
            .bind(&marked)
            .bind(lease.as_secs_f64())
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert!(waited, "the job was sent again before its lease ran out");
    until_row(&w, &job, "received|2|true|false|||false").await;
    s2.finish().await;
    w.finish().await;
}

// Case 10
#[tokio::test]
async fn a_delivery_whose_socket_drops_unanswered_is_released_and_sent_on_the_next_connection() {
    let u = up(LONG).await;
    let t = token(&u).await;
    let job = add_job(&u.w, &u.id, 1, b"again").await;
    let mut client = open(u.s.addr, &t).await;
    let (first, _) = command(&mut client).await;
    until_row(&u.w, &job, SENT).await;
    drop(client);
    until_row(&u.w, &job, "received|1|false|false|||false").await;
    let mut client = open(u.s.addr, &t).await;
    let (second, bytes) = command(&mut client).await;
    assert_ne!(first, second);
    assert_eq!(bytes, b"again");
    until_row(&u.w, &job, "received|2|true|false|||false").await;
    u.finish().await;
}

// Case 11
#[tokio::test]
async fn a_replacement_releases_the_unanswered_delivery_and_only_the_new_id_settles_it() {
    let u = up(LONG).await;
    let t = token(&u).await;
    let a = add_job(&u.w, &u.id, 1, b"a").await;
    let mut old = open(u.s.addr, &t).await;
    let (old_id, _) = command(&mut old).await;
    until_row(&u.w, &a, SENT).await;
    let mut new = open(u.s.addr, &t).await;
    assert_eq!(close_code(&mut old).await, 4001);
    let (new_id, bytes) = command(&mut new).await;
    assert_ne!(old_id, new_id);
    assert_eq!(bytes, b"a");
    until_row(&u.w, &a, "received|2|true|false|||false").await;
    // The old ID on the new connection matches nothing; a later job's ack, read after it, shows
    // that it was handled.
    let b = add_job(&u.w, &u.id, 2, b"b").await;
    let (b_id, _) = command(&mut new).await;
    ack(&mut new, &old_id).await;
    ack(&mut new, &b_id).await;
    until_row(&u.w, &b, DELIVERED).await;
    assert_eq!(row(&u.w, &a).await, "received|2|true|false|||false");
    ack(&mut new, &new_id).await;
    until_row(&u.w, &a, "received|2|true|true|||false").await;
    u.finish().await;
}

// Case 14
#[tokio::test]
async fn a_rejection_ends_the_job_with_the_agents_reason_and_the_next_job_is_delivered() {
    let u = up(LONG).await;
    let a = add_job(&u.w, &u.id, 1, b"a").await;
    let b = add_job(&u.w, &u.id, 2, b"b").await;
    let mut client = open(u.s.addr, &token(&u).await).await;
    let (a_id, _) = command(&mut client).await;
    let (b_id, _) = command(&mut client).await;
    reject(&mut client, &a_id, "badSignature").await;
    ack(&mut client, &b_id).await;
    until_row(&u.w, &b, DELIVERED).await;
    until_row(
        &u.w,
        &a,
        "failed|1|true|false|instructionRejected|badSignature|true",
    )
    .await;
    // Nothing is sent again.
    quiet_for(&mut client, 300).await;
    u.finish().await;
}

// Case 15
#[tokio::test]
async fn an_answer_for_a_finished_or_delivered_job_writes_nothing() {
    let u = up(LONG).await;
    let a = add_job(&u.w, &u.id, 1, b"a").await;
    let c = add_job(&u.w, &u.id, 2, b"c").await;
    let marker = add_job(&u.w, &u.id, 3, b"m").await;
    let mut client = open(u.s.addr, &token(&u).await).await;
    let (a_id, _) = command(&mut client).await;
    let (c_id, _) = command(&mut client).await;
    let (m_id, _) = command(&mut client).await;
    for job in [&a, &c, &marker] {
        until_row(&u.w, job, SENT).await;
    }
    sqlx::query("UPDATE jobs SET finished_at = now(), state = 'cancelled' WHERE id = $1::uuid")
        .bind(&a)
        .execute(&u.w.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET delivered_at = now() WHERE id = $1::uuid")
        .bind(&c)
        .execute(&u.w.pool)
        .await
        .unwrap();
    ack(&mut client, &a_id).await;
    reject(&mut client, &c_id, "malformed").await;
    ack(&mut client, &m_id).await;
    until_row(&u.w, &marker, DELIVERED).await;
    assert_eq!(row(&u.w, &a).await, "cancelled|1|true|false|||true");
    assert_eq!(row(&u.w, &c).await, DELIVERED);
    u.finish().await;
}

// Case 16
#[tokio::test]
async fn a_job_of_a_retired_key_is_ended_connected_or_not_and_an_active_one_is_delivered() {
    let u = up(LONG).await;
    let (other_id, _) = second_agent(&u).await;
    let stale = "0".repeat(64);
    let connected = add_job_with(&u.w, &u.id, 1, b"stale-1", &stale, 3600).await;
    let offline = add_job_with(&u.w, &other_id, 2, b"stale-2", &stale, 3600).await;
    let fresh = add_job(&u.w, &u.id, 3, b"fresh").await;
    let mut client = open(u.s.addr, &token(&u).await).await;
    assert_eq!(command(&mut client).await.1, b"fresh");
    until_row(&u.w, &fresh, SENT).await;
    assert_eq!(row(&u.w, &connected).await, RETIRED);
    assert_eq!(row(&u.w, &offline).await, RETIRED);
    quiet_for(&mut client, 200).await;
    u.finish().await;
}

// Case 16b
#[tokio::test]
async fn with_no_active_key_nothing_is_ended_or_taken_until_one_is_active_again() {
    let passes = Arc::new(AtomicU64::new(0));
    let counted = Settings {
        passes: Some(passes.clone()),
        ..settings(LONG)
    };
    let u = Up::with_queue(2, Some(counted)).await;
    sqlx::query("UPDATE instruction_keys SET retired_at = now()")
        .execute(&u.w.pool)
        .await
        .unwrap();
    let stale = add_job_with(&u.w, &u.id, 1, b"stale", &"0".repeat(64), 3600).await;
    let fresh = add_job(&u.w, &u.id, 2, b"fresh").await;
    let mut client = open(u.s.addr, &token(&u).await).await;
    // Several passes complete after the rows exist; the rows stay as they are.
    let before = passes.load(Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(15);
    while passes.load(Ordering::Relaxed) < before + 5 {
        assert!(Instant::now() < deadline, "no passes ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(row(&u.w, &stale).await, FRESH);
    assert_eq!(row(&u.w, &fresh).await, FRESH);
    sqlx::query("UPDATE instruction_keys SET retired_at = NULL")
        .execute(&u.w.pool)
        .await
        .unwrap();
    assert_eq!(command(&mut client).await.1, b"fresh");
    until_row(&u.w, &stale, RETIRED).await;
    u.finish().await;
}

// Case 17
#[tokio::test]
async fn a_revoked_agent_that_is_still_connected_gets_nothing() {
    let u = up(LONG).await;
    let (other_id, other_token) = second_agent(&u).await;
    let mut revoked = open(u.s.addr, &token(&u).await).await;
    let mut other = open(u.s.addr, &other_token).await;
    revoke_row(&u, &u.id).await;
    let skipped = add_job(&u.w, &u.id, 1, b"revoked").await;
    add_job(&u.w, &other_id, 2, b"other").await;
    assert_eq!(command(&mut other).await.1, b"other");
    assert_eq!(row(&u.w, &skipped).await, FRESH);
    quiet_for(&mut revoked, 200).await;
    u.finish().await;
}

// Cases 18 and 19
#[tokio::test]
async fn a_job_past_its_start_deadline_or_not_received_or_finished_is_not_taken() {
    let u = up(LONG).await;
    let late = add_job_with(&u.w, &u.id, 1, b"late", &u.w.key.key_id(), -10).await;
    let running = add_job(&u.w, &u.id, 2, b"running").await;
    let done = add_job(&u.w, &u.id, 3, b"done").await;
    let live = add_job(&u.w, &u.id, 4, b"live").await;
    sqlx::query("UPDATE jobs SET state = 'running' WHERE id = $1::uuid")
        .bind(&running)
        .execute(&u.w.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET finished_at = now() WHERE id = $1::uuid")
        .bind(&done)
        .execute(&u.w.pool)
        .await
        .unwrap();
    let mut client = open(u.s.addr, &token(&u).await).await;
    assert_eq!(command(&mut client).await.1, b"live");
    until_row(&u.w, &live, SENT).await;
    assert_eq!(row(&u.w, &late).await, FRESH);
    assert_eq!(row(&u.w, &running).await, "running|0|false|false|||false");
    assert_eq!(row(&u.w, &done).await, "received|0|false|false|||true");
    quiet_for(&mut client, 200).await;
    u.finish().await;
}

// Case 20
#[tokio::test]
async fn a_stop_while_the_queue_waits_for_its_tick_ends_it_at_once() {
    let long_period = Settings {
        period: Duration::from_secs(3600),
        lease: LONG,
        passes: None,
    };
    let u = Up::with_queue(2, Some(long_period)).await;
    let Up { w, s, .. } = u;
    let started = Instant::now();
    s.finish().await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the stop waited for the grace period: {:?}",
        started.elapsed()
    );
    w.finish().await;
}

// Case 21
#[tokio::test]
async fn a_failed_pass_sends_nothing_warns_once_keeps_draining_and_the_next_tick_retries() {
    let u = up(LONG).await;
    let (log, _guard) = capture();
    let a = add_job(&u.w, &u.id, 1, b"a").await;
    let t = token(&u).await;
    // A column the queue reads is missing: every pass fails.
    u.w.pg
        .execute("ALTER TABLE jobs RENAME COLUMN delivered_at TO delivered_at_x")
        .await;
    let seen = |log: &crate::redeem::Captured| {
        log.text()
            .lines()
            .filter(|l| l.contains("the delivery pass failed"))
            .count()
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while seen(&log) == 0 {
        assert!(Instant::now() < deadline, "no warning");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut client = open(u.s.addr, &t).await;
    // More ticks pass, the warning is not repeated, nothing is sent, and the inbox is still read:
    // more events than it holds are taken in and the heartbeat after them is echoed.
    for seq in 0..300 {
        client.send(progress(seq)).await.unwrap();
    }
    heartbeat(&mut client).await;
    tokio::time::sleep(PERIOD * 4).await;
    assert_eq!(seen(&log), 1);
    quiet_for(&mut client, 100).await;
    u.w.pg
        .execute("ALTER TABLE jobs RENAME COLUMN delivered_at_x TO delivered_at")
        .await;
    assert_eq!(command(&mut client).await.1, b"a");
    until_row(&u.w, &a, SENT).await;
    u.finish().await;
}

/// A row as the take returns it.
type Taken = (String, String, String, Vec<u8>, i32, Option<String>);

async fn take(u: &Up, agents: &[&str], in_flight: &[&String], limit: i64) -> Vec<Taken> {
    sqlx::query_as::<_, Taken>(TAKE_SQL)
        .bind(agents.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
        .bind(60.0_f64)
        .bind(in_flight.iter().map(|j| (*j).clone()).collect::<Vec<_>>())
        .bind(limit)
        .bind(64_i64)
        .fetch_all(&u.w.pool)
        .await
        .unwrap()
}

/// The text of a job's mark.
async fn mark_of(w: &World, job: &str) -> String {
    sqlx::query_scalar("SELECT coalesce(delivery_sent_at::text, '') FROM jobs WHERE id = $1::uuid")
        .bind(job)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

// Head of line, in the take
#[tokio::test]
async fn the_take_gives_every_agent_its_turn_however_many_older_jobs_one_agent_has() {
    let u = Up::new().await;
    let (other_id, _) = second_agent(&u).await;
    for n in 0..80 {
        add_job(&u.w, &u.id, n, b"x").await;
    }
    let newer = add_job(&u.w, &other_id, 500, b"y").await;
    let taken = take(&u, &[&u.id, &other_id], &[], 16).await;
    assert_eq!(
        taken.len(),
        17,
        "16 of the first agent's and the other's one"
    );
    assert!(taken.iter().any(|row| row.1 == newer));
    assert_eq!(taken.iter().filter(|row| row.2 == u.id).count(), 16);
}

/// `count` jobs for `agent`, all older than anything `add_job` makes, each carrying `bytes`.
async fn add_old_jobs<'e>(
    db: impl sqlx::PgExecutor<'e>,
    w: &World,
    agent: &str,
    count: i32,
    bytes: &[u8],
) {
    sqlx::query(
        "INSERT INTO jobs (tenant_id, id, kind, agent_id, operator_id, vci_id, \
             confirmation_level, start_deadline, signed_job, signing_key_id, created_at, \
             idempotency_key) \
         SELECT a.tenant_id, gen_random_uuid(), 'runSequence', a.id, o.id, 'vci', 'notifyOnly', \
             now() + interval '1 hour', $3, $4, \
             now() - interval '2 hours' + n * interval '1 millisecond', gen_random_uuid()::text \
         FROM agents a JOIN operators o ON o.tenant_id = a.tenant_id, generate_series(1, $2) n \
         WHERE a.id = $1::uuid",
    )
    .bind(agent)
    .bind(count)
    .bind(bytes)
    .bind(w.key.key_id())
    .execute(db)
    .await
    .unwrap();
}

/// `count` agents besides the one `Up` registers: (agent ID, token).
async fn more_agents(u: &Up, count: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for _ in 0..count {
        out.push(second_agent(u).await);
    }
    out
}

/// The take's ranking when the pass limit is filled: five agents with 16 old jobs each (80 rows,
/// more than the pass takes) and a sixth with one newer job. Ordered by age alone the 64 oldest
/// rows would all be the first five agents'; by rank the sixth agent's only job is in the pass.
#[tokio::test]
async fn the_take_that_fills_the_pass_limit_still_includes_an_agents_only_newer_job() {
    let u = Up::new().await;
    let others = more_agents(&u, 5).await;
    let mut ids = vec![u.id.clone()];
    ids.extend(others.iter().take(4).map(|(id, _)| id.clone()));
    for id in &ids {
        add_old_jobs(&u.w.pool, &u.w, id, 16, b"backlog").await;
    }
    let single_agent = &others[4].0;
    let single = add_job(&u.w, single_agent, 500, b"single").await;
    let mut connected: Vec<&str> = ids.iter().map(String::as_str).collect();
    connected.push(single_agent);
    let taken = take(&u, &connected, &[], 16).await;
    assert_eq!(taken.len(), 64, "the pass limit");
    assert!(
        taken.iter().any(|row| row.1 == single),
        "an agent's only job waits behind other agents' second jobs"
    );
}

/// The take is by rank, so more agents with backlogs than a pass holds do not keep another
/// agent's only job out of the first pass. Every connection is open and every row is in before
/// the first pass can see any (the jobs go in one transaction), and a pass's marks commit
/// together, so the first state with a mark is the first pass's whole result.
#[tokio::test]
async fn every_agents_oldest_job_is_sent_in_the_first_pass_however_many_agents_are_connected() {
    let counted = Settings {
        period: Duration::from_millis(500),
        ..settings(LONG)
    };
    let u = Up::with_queue(2, Some(counted)).await;
    let others = more_agents(&u, 5).await;
    let mut clients = vec![open(u.s.addr, &token(&u).await).await];
    for (_, t) in &others {
        clients.push(open(u.s.addr, t).await);
    }
    let mut backlog = vec![u.id.clone()];
    backlog.extend(others.iter().take(4).map(|(id, _)| id.clone()));
    let single_agent = &others[4].0;
    let mut tx = u.w.pool.begin().await.unwrap();
    for id in &backlog {
        add_old_jobs(&mut *tx, &u.w, id, 16, b"backlog").await;
    }
    add_old_jobs(&mut *tx, &u.w, single_agent, 1, b"single").await;
    // The single job is newer than every backlog row.
    sqlx::query("UPDATE jobs SET created_at = now() WHERE agent_id = $1::uuid")
        .bind(single_agent)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // One statement, so one snapshot: (agent, marked rows) once any row is marked.
    let deadline = Instant::now() + Duration::from_secs(30);
    let marked: Vec<(String, i64)> = loop {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT agent_id::text, count(*) FROM jobs WHERE delivery_sent_at IS NOT NULL \
             GROUP BY agent_id",
        )
        .fetch_all(&u.w.pool)
        .await
        .unwrap();
        if !rows.is_empty() {
            break rows;
        }
        assert!(Instant::now() < deadline, "no pass marked anything");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let count = |id: &str| marked.iter().find(|(a, _)| a == id).map_or(0, |(_, n)| *n);
    assert_eq!(count(single_agent), 1, "an agent's only job waits");
    for id in &backlog {
        // 64 rows over six agents by rank: ten whole ranks and four of the eleventh.
        assert!(count(id) >= 10, "a backlog agent got only {}", count(id));
    }
    drop(clients);
    u.finish().await;
}

// The take's in-flight exclusion
#[tokio::test]
async fn the_take_leaves_out_the_jobs_that_are_in_flight_in_the_process() {
    let u = Up::new().await;
    let a = add_job(&u.w, &u.id, 1, b"a").await;
    let b = add_job(&u.w, &u.id, 2, b"b").await;
    assert_eq!(take(&u, &[&u.id], &[], 16).await.len(), 2);
    let taken = take(&u, &[&u.id], &[&a], 16).await;
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].1, b);
    u.finish().await;
}

// The sweep's outstanding-mark clause
#[tokio::test]
async fn the_sweep_leaves_a_job_with_an_outstanding_mark_and_ends_one_whose_mark_expired() {
    let u = up(LONG).await;
    let (other_id, _) = second_agent(&u).await;
    let stale = "0".repeat(64);
    // The rows are made stale by one statement once their marks are written, so that no pass
    // sees them half made.
    let unmarked = add_job(&u.w, &other_id, 1, b"u").await;
    let outstanding = add_job(&u.w, &other_id, 2, b"o").await;
    let expired = add_job(&u.w, &other_id, 3, b"e").await;
    sqlx::query(
        "UPDATE jobs SET delivery_attempts = 1, delivery_sent_at = now() WHERE id = $1::uuid",
    )
    .bind(&outstanding)
    .execute(&u.w.pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE jobs SET delivery_attempts = 1, delivery_sent_at = now() - interval '1 hour' \
         WHERE id = $1::uuid",
    )
    .bind(&expired)
    .execute(&u.w.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET signing_key_id = $1 WHERE id = ANY($2::uuid[])")
        .bind(&stale)
        .bind(vec![unmarked.clone(), outstanding.clone(), expired.clone()])
        .execute(&u.w.pool)
        .await
        .unwrap();
    // One statement ends the stale rows, so the unmarked one shows that the pass ran.
    until_row(&u.w, &unmarked, RETIRED).await;
    assert_eq!(
        row(&u.w, &expired).await,
        "failed|1|true|false|instructionKeyRetired|the instruction was signed by a key that is no longer active|true"
    );
    assert_eq!(row(&u.w, &outstanding).await, SENT);
    u.finish().await;
}

// The release's guard on the attempt number and the mark
#[tokio::test]
async fn a_stale_flights_release_does_not_clear_a_newer_mark() {
    let u = up(LONG).await;
    let t = token(&u).await;
    let newer = add_job(&u.w, &u.id, 1, b"newer").await;
    let bumped = add_job(&u.w, &u.id, 2, b"bumped").await;
    let mut client = open(u.s.addr, &t).await;
    command(&mut client).await;
    command(&mut client).await;
    until_row(&u.w, &newer, SENT).await;
    until_row(&u.w, &bumped, SENT).await;
    // Another instance took the rows since (the first mark's commit had failed): the same
    // attempt number under a later mark, and a later attempt under the same mark.
    sqlx::query(
        "UPDATE jobs SET delivery_sent_at = now() + interval '1 minute' WHERE id = $1::uuid",
    )
    .bind(&newer)
    .execute(&u.w.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET delivery_attempts = 2 WHERE id = $1::uuid")
        .bind(&bumped)
        .execute(&u.w.pool)
        .await
        .unwrap();
    let newer_mark = mark_of(&u.w, &newer).await;
    // The connection leaves, which releases its flights; the job after it is delivered on the
    // next connection only once that has been done.
    drop(client);
    add_job(&u.w, &u.id, 3, b"marker").await;
    let mut client = open(u.s.addr, &t).await;
    assert_eq!(command(&mut client).await.1, b"marker");
    assert_eq!(row(&u.w, &newer).await, SENT);
    assert_eq!(mark_of(&u.w, &newer).await, newer_mark);
    assert_eq!(row(&u.w, &bumped).await, "received|2|true|false|||false");
    u.finish().await;
}

// The rejection's and the acknowledgement's guard on the mark
#[tokio::test]
async fn a_stale_flights_rejection_and_acknowledgement_do_not_touch_a_newer_mark() {
    let u = up(LONG).await;
    let rejected = add_job(&u.w, &u.id, 1, b"r").await;
    let acked = add_job(&u.w, &u.id, 2, b"a").await;
    let marker = add_job(&u.w, &u.id, 3, b"m").await;
    let mut client = open(u.s.addr, &token(&u).await).await;
    let (r_id, _) = command(&mut client).await;
    let (a_id, _) = command(&mut client).await;
    let (m_id, _) = command(&mut client).await;
    for job in [&rejected, &acked, &marker] {
        until_row(&u.w, job, SENT).await;
    }
    for job in [&rejected, &acked] {
        sqlx::query(
            "UPDATE jobs SET delivery_sent_at = now() + interval '1 minute' WHERE id = $1::uuid",
        )
        .bind(job)
        .execute(&u.w.pool)
        .await
        .unwrap();
    }
    reject(&mut client, &r_id, "badSignature").await;
    ack(&mut client, &a_id).await;
    ack(&mut client, &m_id).await;
    until_row(&u.w, &marker, DELIVERED).await;
    assert_eq!(row(&u.w, &rejected).await, SENT);
    assert_eq!(row(&u.w, &acked).await, SENT);
    u.finish().await;
}

/// Makes the commit of a transaction that changes a job's mark fail, as a lost connection would.
async fn refuse_marks(u: &Up) {
    u.w.pg
        .execute(
            "CREATE OR REPLACE FUNCTION test_commit_refused() RETURNS trigger LANGUAGE plpgsql \
             AS $$ BEGIN RAISE EXCEPTION 'test: commit refused'; END $$",
        )
        .await;
    u.w.pg
        .execute(
            "CREATE CONSTRAINT TRIGGER test_marks_refused AFTER UPDATE ON jobs DEFERRABLE \
             INITIALLY DEFERRED FOR EACH ROW \
             WHEN (NEW.delivery_sent_at IS DISTINCT FROM OLD.delivery_sent_at) \
             EXECUTE FUNCTION test_commit_refused()",
        )
        .await;
}

/// Makes the commit of a transaction that settles a job's delivery fail.
async fn refuse_answers(u: &Up) {
    u.w.pg
        .execute(
            "CREATE OR REPLACE FUNCTION test_commit_refused() RETURNS trigger LANGUAGE plpgsql \
             AS $$ BEGIN RAISE EXCEPTION 'test: commit refused'; END $$",
        )
        .await;
    u.w.pg
        .execute(
            "CREATE CONSTRAINT TRIGGER test_answers_refused AFTER UPDATE ON jobs DEFERRABLE \
             INITIALLY DEFERRED FOR EACH ROW \
             WHEN (NEW.delivered_at IS DISTINCT FROM OLD.delivered_at) \
             EXECUTE FUNCTION test_commit_refused()",
        )
        .await;
}

async fn until_logged(log: &crate::redeem::Captured, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !log.text().contains(what) {
        assert!(Instant::now() < deadline, "not logged: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A delivery whose mark's commit failed after the frame was queued, then the agent's answer. The
/// row had no mark before (`old` false) or an expired one from an earlier delivery.
async fn answer_after_an_uncommitted_mark(old: bool, rejection: bool) {
    let u = up(LONG).await;
    let (log, _guard) = capture();
    let job = add_job(&u.w, &u.id, 1, b"a").await;
    if old {
        sqlx::query(
            "UPDATE jobs SET delivery_attempts = 1, delivery_sent_at = now() - interval '1 hour' \
             WHERE id = $1::uuid",
        )
        .bind(&job)
        .execute(&u.w.pool)
        .await
        .unwrap();
    }
    let before = mark_of(&u.w, &job).await;
    refuse_marks(&u).await;
    let mut client = open(u.s.addr, &token(&u).await).await;
    let (id, _) = command(&mut client).await;
    until_logged(&log, "could not mark deliveries").await;
    u.w.pg
        .execute("DROP TRIGGER test_marks_refused ON jobs")
        .await;
    // The row is as it was before the send.
    assert_eq!(mark_of(&u.w, &job).await, before);
    if rejection {
        reject(&mut client, &id, "badSignature").await;
        let attempts = u32::from(old);
        let sent = old;
        let expected =
            format!("failed|{attempts}|{sent}|false|instructionRejected|badSignature|true");
        until_row(&u.w, &job, &expected).await;
    } else {
        ack(&mut client, &id).await;
        let expected = if old {
            "received|2|true|true|||false"
        } else {
            "received|1|false|true|||false"
        };
        until_row(&u.w, &job, expected).await;
    }
    u.finish().await;
}

// The acknowledgement's raising of the attempts
#[tokio::test]
async fn an_acknowledgement_after_a_mark_that_was_not_committed_raises_the_attempts() {
    answer_after_an_uncommitted_mark(false, false).await;
}

#[tokio::test]
async fn an_acknowledgement_after_a_mark_that_was_not_committed_settles_a_row_with_an_old_mark() {
    answer_after_an_uncommitted_mark(true, false).await;
}

#[tokio::test]
async fn a_rejection_after_a_mark_that_was_not_committed_ends_a_row_with_no_mark() {
    answer_after_an_uncommitted_mark(false, true).await;
}

#[tokio::test]
async fn a_rejection_after_a_mark_that_was_not_committed_ends_a_row_with_an_old_mark() {
    answer_after_an_uncommitted_mark(true, true).await;
}

/// Like [`refuse_marks`], and the refused mark's time is kept in a sequence (which a rollback does
/// not undo), so that a test can stage the row as a commit that went through though it was
/// reported as failed would have left it.
async fn refuse_marks_keeping_the_stamp(u: &Up) {
    u.w.pg
        .execute("CREATE SEQUENCE test_mark_stamp MINVALUE 0")
        .await;
    u.w.pg
        .execute(
            "CREATE OR REPLACE FUNCTION test_commit_refused() RETURNS trigger LANGUAGE plpgsql \
             AS $$ BEGIN \
               PERFORM setval('test_mark_stamp', \
                 (extract(epoch FROM NEW.delivery_sent_at)::numeric * 1000000)::bigint); \
               RAISE EXCEPTION 'test: commit refused'; END $$",
        )
        .await;
    u.w.pg
        .execute(
            "CREATE CONSTRAINT TRIGGER test_marks_refused AFTER UPDATE ON jobs DEFERRABLE \
             INITIALLY DEFERRED FOR EACH ROW \
             WHEN (NEW.delivery_sent_at IS DISTINCT FROM OLD.delivery_sent_at) \
             EXECUTE FUNCTION test_commit_refused()",
        )
        .await;
}

/// A delivery whose mark's commit was reported as failed, though it went through: the row carries
/// the mark the database returned, and one more attempt, when the agent's answer comes. The row
/// had no mark before (`old` false) or an expired one.
async fn answer_after_a_mark_that_was_reported_failed(old: bool, rejection: bool) {
    let u = up(LONG).await;
    let (log, _guard) = capture();
    let job = add_job(&u.w, &u.id, 1, b"a").await;
    if old {
        sqlx::query(
            "UPDATE jobs SET delivery_attempts = 1, delivery_sent_at = now() - interval '1 hour' \
             WHERE id = $1::uuid",
        )
        .bind(&job)
        .execute(&u.w.pool)
        .await
        .unwrap();
    }
    refuse_marks_keeping_the_stamp(&u).await;
    let mut client = open(u.s.addr, &token(&u).await).await;
    let (id, _) = command(&mut client).await;
    until_logged(&log, "could not mark deliveries").await;
    u.w.pg
        .execute("DROP TRIGGER test_marks_refused ON jobs")
        .await;
    // The commit "went through": the row carries the returned mark and the next attempt.
    sqlx::query(
        "UPDATE jobs SET delivery_attempts = delivery_attempts + 1, \
             delivery_sent_at = 'epoch'::timestamptz \
                 + (SELECT last_value FROM test_mark_stamp) * interval '1 microsecond' \
         WHERE id = $1::uuid",
    )
    .bind(&job)
    .execute(&u.w.pool)
    .await
    .unwrap();
    let attempts = if old { 2 } else { 1 };
    if rejection {
        reject(&mut client, &id, "badSignature").await;
        let expected =
            format!("failed|{attempts}|true|false|instructionRejected|badSignature|true");
        until_row(&u.w, &job, &expected).await;
    } else {
        ack(&mut client, &id).await;
        until_row(
            &u.w,
            &job,
            &format!("received|{attempts}|true|true|||false"),
        )
        .await;
    }
    u.finish().await;
}

#[tokio::test]
async fn an_acknowledgement_settles_a_row_that_carries_the_mark_of_a_commit_reported_as_failed() {
    answer_after_a_mark_that_was_reported_failed(false, false).await;
}

#[tokio::test]
async fn a_rejection_ends_a_row_that_carries_the_mark_of_a_commit_reported_as_failed_and_old() {
    answer_after_a_mark_that_was_reported_failed(true, true).await;
}

// The carry: an answer that could not be written is kept, retried, and its job stays out of the take
#[tokio::test]
async fn an_answer_that_could_not_be_written_is_retried_and_its_job_is_not_sent_again_meanwhile() {
    // A lease that runs out while the answer is unwritten: only the take's exclusion of the carried
    // job keeps the expired mark from being taken again.
    let lease = Duration::from_secs(1);
    let passes = Arc::new(AtomicU64::new(0));
    let counted = Settings {
        passes: Some(passes.clone()),
        ..settings(lease)
    };
    let u = Up::with_queue(2, Some(counted)).await;
    let (log, _guard) = capture();
    let job = add_job(&u.w, &u.id, 1, b"a").await;
    let mut client = open(u.s.addr, &token(&u).await).await;
    let (id, _) = command(&mut client).await;
    until_row(&u.w, &job, SENT).await;
    refuse_answers(&u).await;
    ack(&mut client, &id).await;
    until_logged(&log, "could not record answers").await;
    // Passes keep running for longer than the lease, the write keeps failing, and the job is
    // neither sent again nor settled.
    tokio::time::sleep(lease + lease / 2).await;
    let before = passes.load(Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(15);
    while passes.load(Ordering::Relaxed) < before + 5 {
        assert!(Instant::now() < deadline, "no passes ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    quiet_for(&mut client, 100).await;
    assert_eq!(row(&u.w, &job).await, SENT);
    // The database accepts the write again: the kept answer is written without another ack.
    u.w.pg
        .execute("DROP TRIGGER test_answers_refused ON jobs")
        .await;
    until_row(&u.w, &job, DELIVERED).await;
    quiet_for(&mut client, 100).await;
    u.finish().await;
}

// The inbox is read while the pass waits for the database
#[tokio::test]
async fn the_inbox_is_still_read_while_a_pass_waits_for_a_lock_on_the_jobs_table() {
    let u = up(LONG).await;
    let t = token(&u).await;
    let mut lock = u.w.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE jobs IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    // A pass is blocked on the lock (the database's lock timeout is 5 seconds, so it is blocked
    // for the rest of this test's reads).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let waiting =
            u.w.pg
                .scalar(
                    "SELECT count(*) FROM pg_stat_activity WHERE pid <> pg_backend_pid() \
                 AND wait_event_type = 'Lock' AND query LIKE 'UPDATE jobs SET state%'",
                )
                .await;
        if waiting != "0" {
            break;
        }
        assert!(Instant::now() < deadline, "no pass is waiting");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut client = open(u.s.addr, &t).await;
    // More events than the inbox holds, then a heartbeat: the connection keeps reading only if the
    // queue keeps taking from the inbox while it waits.
    for seq in 0..300 {
        client.send(progress(seq)).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(3), heartbeat(&mut client))
        .await
        .expect("the heartbeat is echoed while the pass is still blocked");
    lock.rollback().await.unwrap();
    let job = add_job(&u.w, &u.id, 1, b"after").await;
    assert_eq!(command(&mut client).await.1, b"after");
    until_row(&u.w, &job, SENT).await;
    u.finish().await;
}

/// `Server::start` turns the queue on, `Server::bind` leaves it off.
#[tokio::test]
async fn start_turns_the_queue_on_and_bind_leaves_it_off() {
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
    assert!(started.job_queue_enabled());
    Running::spawn(started).finish().await;
    let bound = Server::bind(&config.http, pg.pool(), instruction_key())
        .await
        .unwrap();
    assert!(!bound.job_queue_enabled());
}

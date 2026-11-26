//! The server's delivery of communication-log requests and its application of the agents'
//! answers against a throwaway PostgreSQL (design 8.11, ADR-508 items 3 and 4): fake agents are
//! WebSocket clients of a server whose delivery queue and communication-log task run with a short
//! period. The requests are rows the test inserts, or are made through the Web API
//! (`comm_log_uploads.rs`) where a test is about a request that is written while a command or an
//! answer is in flight.
//!
//! The tests run on the current-thread runtime and wait in real time. A negative (a frame that
//! must not come, a row that must not change) is asserted only after something positive that
//! comes later in the same order has been seen: a frame for another request, an answer applied
//! after the one that must change nothing, or a count of finished passes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::SinkExt;
use server::comm_log_requests::Settings;
use server::hub::OUTBOUND_CAPACITY;
use shared_proto::ServerMessage;
use tokio_tungstenite::tungstenite::Message;

use crate::comm_log_uploads::{Seed, alice, job_of, object, request, row};
use crate::control::{Client, heartbeat, open, quiet_for, second_agent, text, token};
use crate::job_queue::{LONG, PERIOD, settings};
use crate::session::Up;

const LIMIT: Duration = Duration::from_secs(15);
/// A lease the tests that resend wait for.
const SHORT: Duration = Duration::from_millis(300);

struct Rig {
    u: Up,
    passes: Arc<AtomicU64>,
    failures: Arc<AtomicU64>,
}

impl Rig {
    async fn new(lease: Duration) -> Rig {
        Rig::with_grace(2, lease).await
    }

    async fn with_grace(grace_secs: u64, lease: Duration) -> Rig {
        let passes = Arc::new(AtomicU64::new(0));
        let failures = Arc::new(AtomicU64::new(0));
        let comm_log = Settings {
            period: PERIOD,
            lease,
            passes: Some(passes.clone()),
            failures: Some(failures.clone()),
        };
        Rig {
            u: Up::with_comm_log(grace_secs, settings(LONG), comm_log).await,
            passes,
            failures,
        }
    }

    async fn connect(&self) -> Client {
        open(self.u.s.addr, &token(&self.u).await).await
    }

    /// A job of `agent` that was delivered and has ended, so that the queue does not deliver it
    /// and an operator may ask for its log.
    async fn job(&self, agent: &str) -> String {
        job_of(&self.u, agent).await
    }

    /// A request for the job's log at generation 1, in `state`, made `age_hours` ago.
    async fn request(&self, job: &str, state: &str, age_hours: i64) {
        Seed {
            age_secs: age_hours * 3600,
            ..Seed::new(state)
        }
        .put(&self.u, job)
        .await;
    }

    /// `n` requests for generations 1 to `n` of the job, all in state `requested`.
    async fn requests(&self, job: &str, n: i64) {
        sqlx::query(
            "INSERT INTO comm_log_requests (tenant_id, job_id, generation, agent_id, requested_by) \
             SELECT tenant_id, id, g, agent_id, operator_id \
             FROM jobs, generate_series(1, $2::bigint) g WHERE id = $1::uuid",
        )
        .bind(job)
        .bind(n)
        .execute(&self.u.w.pool)
        .await
        .unwrap();
    }

    /// A `commLog` object of the job at generation 1 in `state`.
    async fn object(&self, job: &str, state: &str) {
        object(&self.u, job, 1, "commLog", state).await;
    }

    /// The row as one line: state, refusal, whether sent, whether answered.
    async fn row(&self, job: &str) -> String {
        row(&self.u, job).await
    }

    async fn until_row(&self, job: &str, expected: &str) {
        let deadline = Instant::now() + LIMIT;
        loop {
            let now = self.row(job).await;
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

    /// When the command was last sent, in seconds since the epoch, as text.
    async fn sent_at(&self, job: &str) -> String {
        sqlx::query_scalar(
            "SELECT coalesce(extract(epoch FROM sent_at)::text, '') FROM comm_log_requests \
             WHERE job_id = $1::uuid",
        )
        .bind(job)
        .fetch_one(&self.u.w.pool)
        .await
        .unwrap()
    }

    async fn until_sent_after(&self, job: &str, before: &str) -> String {
        let deadline = Instant::now() + LIMIT;
        loop {
            let now = self.sent_at(job).await;
            if now != before {
                return now;
            }
            assert!(Instant::now() < deadline, "sent_at did not move");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until `n` more passes have finished, each of which saw every row as it is now.
    async fn passes(&self, n: u64) {
        let target = self.passes.load(Ordering::Relaxed) + n;
        let deadline = Instant::now() + LIMIT;
        while self.passes.load(Ordering::Relaxed) < target {
            assert!(Instant::now() < deadline, "the passes did not come");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn finish(self) {
        self.u.finish().await;
    }
}

/// The next frame, which must be a `logUploadCommand`: its job and generation.
async fn command(client: &mut Client) -> (String, u64) {
    match ServerMessage::decode(&text(client).await).unwrap() {
        ServerMessage::LogUploadCommand { job_id, generation } => (job_id.0, generation),
        other => panic!("a logUploadCommand was due: {other:?}"),
    }
}

async fn answer(client: &mut Client, job: &str, outcome: &str) {
    let body = format!(
        r#"{{"type":"logUploadAnswer","jobId":"{job}","generation":1,"outcome":"{outcome}"}}"#
    );
    client.send(Message::Text(body.into())).await.unwrap();
}

#[tokio::test]
async fn a_request_of_a_connected_agent_is_sent_and_stamped_and_not_sent_again_within_the_lease() {
    let r = Rig::new(LONG).await;
    let job = r.job(&r.u.id).await;
    r.request(&job, "requested", 0).await;
    let mut client = r.connect().await;
    assert_eq!(command(&mut client).await, (job.clone(), 1));
    r.until_row(&job, "requested|-|true|false").await;
    let stamped = r.sent_at(&job).await;
    // Two whole passes after the stamp: a resend would have been queued by now.
    r.passes(3).await;
    quiet_for(&mut client, 100).await;
    assert_eq!(r.sent_at(&job).await, stamped);
    r.finish().await;
}

#[tokio::test]
async fn queued_makes_the_row_accepted_and_a_refusal_makes_it_refused_with_its_reason() {
    let r = Rig::new(LONG).await;
    let mut jobs = Vec::new();
    for _ in 0..5 {
        let job = r.job(&r.u.id).await;
        r.request(&job, "requested", 0).await;
        jobs.push(job);
    }
    let mut client = r.connect().await;
    for _ in 0..jobs.len() {
        command(&mut client).await;
    }
    // An answer applies only to a row that has been sent (ADR-508 item 4).
    for job in &jobs {
        r.until_row(job, "requested|-|true|false").await;
    }
    for (job, outcome) in jobs
        .iter()
        .zip(["queued", "unknownJob", "noLog", "busy", "tooLarge"])
    {
        answer(&mut client, job, outcome).await;
    }
    let expected = [
        "accepted|-|true|true",
        "refused|unknownJob|true|true",
        "refused|noLog|true|true",
        "refused|busy|true|true",
        "refused|tooLarge|true|true",
    ];
    for (job, expected) in jobs.iter().zip(expected) {
        r.until_row(job, expected).await;
    }
    r.finish().await;
}

#[tokio::test]
async fn an_answer_of_another_agent_of_the_same_or_another_tenant_changes_nothing() {
    let r = Rig::new(LONG).await;
    let (_, same_token) = second_agent(&r.u).await;
    let (other_id, other_token) = second_agent(&r.u).await;
    // The third agent works for another tenant.
    sqlx::query(
        "WITH t AS (INSERT INTO tenants (id, name) VALUES (gen_random_uuid(), 'other') RETURNING id) \
         UPDATE agents SET tenant_id = (SELECT id FROM t) WHERE id = $1::uuid",
    )
    .bind(&other_id)
    .execute(&r.u.w.pool)
    .await
    .unwrap();
    let job = r.job(&r.u.id).await;
    r.request(&job, "requested", 0).await;
    let mut owner = r.connect().await;
    let mut same = open(r.u.s.addr, &same_token).await;
    let mut other = open(r.u.s.addr, &other_token).await;
    assert_eq!(command(&mut owner).await.0, job);
    r.until_row(&job, "requested|-|true|false").await;
    // Each answer is applied in the order it reaches the task, and the owner's comes last: a
    // foreign answer that had changed the row would leave `accepted`, not `refused`.
    answer(&mut same, &job, "queued").await;
    answer(&mut other, &job, "queued").await;
    answer(&mut same, &job, "busy").await;
    answer(&mut other, &job, "tooLarge").await;
    // The echo of a heartbeat shows the frames before it were read, so the owner's answer is
    // queued behind the foreign ones.
    heartbeat(&mut same).await;
    heartbeat(&mut other).await;
    answer(&mut owner, &job, "noLog").await;
    r.until_row(&job, "refused|noLog|true|true").await;
    r.finish().await;
}

#[tokio::test]
async fn an_answer_for_an_unknown_row_or_an_answered_row_changes_nothing_and_the_task_goes_on() {
    let r = Rig::new(LONG).await;
    let first = r.job(&r.u.id).await;
    let second = r.job(&r.u.id).await;
    r.request(&first, "requested", 0).await;
    r.request(&second, "requested", 0).await;
    let mut client = r.connect().await;
    command(&mut client).await;
    command(&mut client).await;
    r.until_row(&first, "requested|-|true|false").await;
    r.until_row(&second, "requested|-|true|false").await;
    answer(&mut client, &first, "queued").await;
    r.until_row(&first, "accepted|-|true|true").await;
    // A later answer for the answered row, one for a job nobody asked for, one that is no
    // canonical UUID and one beyond what the table holds: none changes a row.
    answer(&mut client, &first, "busy").await;
    answer(
        &mut client,
        "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f",
        "queued",
    )
    .await;
    answer(&mut client, "NOT-A-UUID", "queued").await;
    let body = format!(
        r#"{{"type":"logUploadAnswer","jobId":"{second}","generation":18446744073709551615,"outcome":"queued"}}"#
    );
    client.send(Message::Text(body.into())).await.unwrap();
    // The task still applies the next answer, which comes after all of them.
    answer(&mut client, &second, "busy").await;
    r.until_row(&second, "refused|busy|true|true").await;
    assert_eq!(r.row(&first).await, "accepted|-|true|true");
    r.finish().await;
}

#[tokio::test]
async fn an_unanswered_command_is_sent_again_after_the_lease_and_the_stamp_advances() {
    let r = Rig::new(SHORT).await;
    let job = r.job(&r.u.id).await;
    r.request(&job, "requested", 0).await;
    let mut client = r.connect().await;
    assert_eq!(command(&mut client).await.0, job);
    r.until_row(&job, "requested|-|true|false").await;
    let first = r.sent_at(&job).await;
    assert_eq!(command(&mut client).await.0, job, "sent again");
    let second = r.until_sent_after(&job, &first).await;
    assert!(
        second.parse::<f64>().unwrap() > first.parse::<f64>().unwrap(),
        "sent_at advances"
    );
    r.finish().await;
}

#[tokio::test]
async fn a_request_of_an_agent_that_is_not_connected_waits_and_is_sent_when_it_connects() {
    let r = Rig::new(LONG).await;
    let job = r.job(&r.u.id).await;
    r.request(&job, "requested", 0).await;
    let t = token(&r.u).await;
    r.passes(3).await;
    assert_eq!(r.row(&job).await, "requested|-|false|false");
    let mut client = open(r.u.s.addr, &t).await;
    assert_eq!(command(&mut client).await.0, job);
    r.until_row(&job, "requested|-|true|false").await;
    r.finish().await;
}

#[tokio::test]
async fn a_request_a_day_old_expires_unsent_and_a_late_queued_leaves_it_expired() {
    let r = Rig::new(SHORT).await;
    let old = r.job(&r.u.id).await;
    let live = r.job(&r.u.id).await;
    r.request(&old, "requested", 25).await;
    r.request(&live, "requested", 23).await;
    let mut client = r.connect().await;
    r.until_row(&old, "expired|-|false|false").await;
    // Only the request that is not expired is sent, and it is sent again after the lease: by
    // the second frame a pass has judged the old row since it expired.
    assert_eq!(command(&mut client).await.0, live);
    assert_eq!(command(&mut client).await.0, live);
    answer(&mut client, &old, "queued").await;
    answer(&mut client, &live, "queued").await;
    r.until_row(&live, "accepted|-|true|true").await;
    assert_eq!(r.row(&old).await, "expired|-|false|false");
    r.finish().await;
}

#[tokio::test]
async fn answered_and_expired_rows_and_rows_with_a_live_object_are_never_sent() {
    let r = Rig::new(SHORT).await;
    let mut skipped = Vec::new();
    for state in ["accepted", "refused", "expired"] {
        let job = r.job(&r.u.id).await;
        r.request(&job, state, 0).await;
        skipped.push(job);
    }
    for object in ["open", "completing", "finished"] {
        let job = r.job(&r.u.id).await;
        r.request(&job, "requested", 0).await;
        r.object(&job, object).await;
        skipped.push(job);
    }
    // A failed object is not live: the begin gate does not count it, and this request is sent.
    let failed = r.job(&r.u.id).await;
    r.request(&failed, "requested", 0).await;
    r.object(&failed, "failed").await;
    let mut client = r.connect().await;
    // Twice, so that a pass after the first send has judged the others again.
    assert_eq!(command(&mut client).await.0, failed);
    assert_eq!(command(&mut client).await.0, failed);
    for job in &skipped {
        let sent: String = sqlx::query_scalar(
            "SELECT (sent_at IS NOT NULL)::text FROM comm_log_requests WHERE job_id = $1::uuid",
        )
        .bind(job)
        .fetch_one(&r.u.w.pool)
        .await
        .unwrap();
        assert_eq!(sent, "false");
    }
    r.finish().await;
}

#[tokio::test]
async fn a_stop_during_a_pass_ends_the_task_and_the_cut_off_pass_is_not_a_failure() {
    // A grace above the statement's 5 s lock timeout: a task that ignored the stop would hold the
    // run until the statement failed.
    let r = Rig::with_grace(8, LONG).await;
    let job = r.job(&r.u.id).await;
    r.request(&job, "requested", 0).await;
    let Rig {
        u,
        passes,
        failures,
    } = r;
    let Up { w, s, .. } = u;
    let mut tx = w.pool.begin().await.unwrap();
    sqlx::raw_sql("LOCK TABLE comm_log_requests IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE pid <> pg_backend_pid() \
             AND wait_event_type = 'Lock' AND query LIKE 'UPDATE comm_log_requests SET state%'",
        )
        .fetch_one(&w.pool)
        .await
        .unwrap();
        if waiting > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "no pass is waiting on the lock");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let seen = passes.load(Ordering::Relaxed);
    // The pool closes only after the hub's tasks have ended; the statement's own wait on the
    // database is bounded by its 5 s timeout, which the shutdown may still wait for.
    s.finish().await;
    tx.rollback().await.unwrap();
    // The pass the stop cut off is not counted, neither as finished nor as failed.
    assert_eq!(passes.load(Ordering::Relaxed), seen);
    assert_eq!(failures.load(Ordering::Relaxed), 0);
    w.finish().await;
}

#[tokio::test]
async fn an_agent_with_a_backlog_does_not_keep_another_agents_row_from_the_first_passes() {
    let r = Rig::new(LONG).await;
    let (other_id, other_token) = second_agent(&r.u).await;
    let busy_job = r.job(&r.u.id).await;
    // More rows than a pass reads in all, older than the other agent's.
    r.requests(&busy_job, 250).await;
    let other_job = r.job(&other_id).await;
    r.request(&other_job, "requested", 0).await;
    // No pass that finished before this count can have sent the busy agent anything.
    let before = r.passes.load(Ordering::Relaxed);
    let mut busy = r.connect().await;
    let mut other = open(r.u.s.addr, &other_token).await;
    // Two passes: one has started after both connected. At 100 rows a pass, an agent served in
    // the order of the rows would have had the other's row only in the third.
    r.passes(2).await;
    assert_eq!(command(&mut other).await.0, other_job);
    r.until_row(&other_job, "requested|-|true|false").await;
    // The busy agent's rows go out a queue's worth at a time, never more than a pass sent. The
    // connection's writer drains its queue into the socket although the test reads nothing, so
    // every pass may send another queue's worth: the bound counts the passes that ran, plus the
    // one that may have stamped its rows and not yet been counted.
    let sent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM comm_log_requests WHERE job_id = $1::uuid AND sent_at IS NOT NULL",
    )
    .bind(&busy_job)
    .fetch_one(&r.u.w.pool)
    .await
    .unwrap();
    let ran = r.passes.load(Ordering::Relaxed) - before + 1;
    let queue = i64::try_from(OUTBOUND_CAPACITY).unwrap();
    let bound = queue * i64::try_from(ran).unwrap();
    assert!(
        sent > 0 && sent <= bound,
        "{sent} rows stamped in at most {ran} passes"
    );
    let _ = command(&mut busy).await;
    r.finish().await;
}

#[tokio::test]
async fn a_revoked_agent_that_is_still_connected_gets_no_command_and_its_answer_changes_nothing() {
    let r = Rig::new(LONG).await;
    let (revoked_id, revoked_token) = second_agent(&r.u).await;
    let revoked_job = r.job(&revoked_id).await;
    let live_job = r.job(&r.u.id).await;
    let mut revoked = open(r.u.s.addr, &revoked_token).await;
    let mut live = r.connect().await;
    sqlx::query("UPDATE agents SET revoked_at = now() WHERE id = $1::uuid")
        .bind(&revoked_id)
        .execute(&r.u.w.pool)
        .await
        .unwrap();
    r.request(&revoked_job, "requested", 0).await;
    r.request(&live_job, "requested", 0).await;
    // The live agent's command shows a pass has judged both rows, and a second one the repeat.
    assert_eq!(command(&mut live).await.0, live_job);
    r.until_row(&live_job, "requested|-|true|false").await;
    r.passes(2).await;
    assert_eq!(r.row(&revoked_job).await, "requested|-|false|false");
    // Its answer is applied after the revoked agent's, which was sent first.
    answer(&mut revoked, &revoked_job, "queued").await;
    heartbeat(&mut live).await;
    answer(&mut live, &live_job, "queued").await;
    r.until_row(&live_job, "accepted|-|true|true").await;
    assert_eq!(r.row(&revoked_job).await, "requested|-|false|false");
    r.finish().await;
}

#[tokio::test]
async fn a_reconnect_inside_the_lease_gets_the_command_again_on_the_new_connection() {
    let r = Rig::new(LONG).await;
    let job = r.job(&r.u.id).await;
    r.request(&job, "requested", 0).await;
    let t = token(&r.u).await;
    let mut first = open(r.u.s.addr, &t).await;
    assert_eq!(command(&mut first).await.0, job);
    r.until_row(&job, "requested|-|true|false").await;
    let stamped = r.sent_at(&job).await;
    // A new session of the agent replaces the first connection: the lease is 10 minutes.
    let mut second = open(r.u.s.addr, &t).await;
    assert_eq!(command(&mut second).await.0, job);
    r.until_sent_after(&job, &stamped).await;
    r.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The Web API's writes beside the delivery (ADR-508 items 1 and 4)

#[tokio::test]
async fn an_answer_and_a_request_for_one_row_each_take_effect_in_the_order_they_come() {
    // X4: the answer first, the renewal after; and the request first, the answer after.
    let r = Rig::new(LONG).await;
    let alice = alice(&r.u).await;
    let answered = r.job(&r.u.id).await;
    let asked = r.job(&r.u.id).await;
    r.request(&answered, "requested", 0).await;
    r.request(&asked, "requested", 0).await;
    let mut client = r.connect().await;
    for _ in 0..2 {
        command(&mut client).await;
    }
    r.until_row(&answered, "requested|-|true|false").await;
    r.until_row(&asked, "requested|-|true|false").await;
    // The answer first: the row is refused, and the request renews it and sends it again.
    answer(&mut client, &answered, "busy").await;
    r.until_row(&answered, "refused|busy|true|true").await;
    assert_eq!(request(&r.u, &alice, &[&answered]).await.status, 202);
    assert_eq!(command(&mut client).await.0, answered, "sent again");
    r.until_row(&answered, "requested|-|true|false").await;
    // The request first: the row is left, and the answer then applies.
    assert_eq!(request(&r.u, &alice, &[&asked]).await.status, 202);
    assert_eq!(r.row(&asked).await, "requested|-|true|false");
    answer(&mut client, &asked, "busy").await;
    r.until_row(&asked, "refused|busy|true|true").await;
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'commLogUploadRequested'",
    )
    .fetch_one(&r.u.w.pool)
    .await
    .unwrap();
    assert_eq!(audits, 1, "only the renewal");
    r.finish().await;
}

#[tokio::test]
async fn a_request_and_the_expiry_of_one_row_take_effect_in_the_order_they_come() {
    // X5, P1: the expiry first, then the renewal, whose new day is not cut short; and the
    // request first, which leaves the row and its first time, so that it expires on schedule.
    let r = Rig::new(LONG).await;
    let alice = alice(&r.u).await;
    let expired = r.job(&r.u.id).await;
    let left = r.job(&r.u.id).await;
    r.request(&expired, "requested", 25).await;
    r.until_row(&expired, "expired|-|false|false").await;
    assert_eq!(request(&r.u, &alice, &[&expired]).await.status, 202);
    assert_eq!(r.row(&expired).await, "requested|-|false|false");
    r.passes(3).await;
    assert_eq!(
        r.row(&expired).await,
        "requested|-|false|false",
        "a new day"
    );
    // A fresh request: the request leaves its first time, and it expires once that time is a
    // day past.
    Seed::new("requested").put(&r.u, &left).await;
    let at = |job: &str| {
        let job = job.to_owned();
        let pool = r.u.w.pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT requested_at::text FROM comm_log_requests WHERE job_id = $1::uuid",
            )
            .bind(job)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let first = at(&left).await;
    assert_eq!(request(&r.u, &alice, &[&left]).await.status, 202);
    assert_eq!(at(&left).await, first, "the first time is kept");
    sqlx::query(
        "UPDATE comm_log_requests SET requested_at = requested_at - interval '25 hours' \
         WHERE job_id = $1::uuid",
    )
    .bind(&left)
    .execute(&r.u.w.pool)
    .await
    .unwrap();
    r.until_row(&left, "expired|-|false|false").await;
    r.finish().await;
}

#[tokio::test]
async fn a_request_left_as_it_is_is_still_sent_and_stamped_when_the_agent_connects() {
    // X6: a request that is repeated while the agent is away changes nothing, and the delivery
    // that follows stamps it.
    let r = Rig::new(LONG).await;
    let alice = alice(&r.u).await;
    let job = r.job(&r.u.id).await;
    assert_eq!(request(&r.u, &alice, &[&job]).await.status, 202);
    assert_eq!(r.row(&job).await, "requested|-|false|false");
    assert_eq!(request(&r.u, &alice, &[&job]).await.status, 202);
    let mut client = r.connect().await;
    assert_eq!(command(&mut client).await.0, job);
    r.until_row(&job, "requested|-|true|false").await;
    r.finish().await;
}

#[tokio::test]
async fn an_answer_to_a_command_sent_before_a_renewal_is_ignored_until_the_row_is_sent_again() {
    // X7, X8, X9, X10, X11: a live object keeps the renewed rows from being sent until the test
    // frees them, so that the stale answers arrive while the rows are unsent. The answers of a
    // sent row that is not renewed (X12) are the tests above.
    let r = Rig::new(LONG).await;
    let alice = alice(&r.u).await;
    let refused = r.job(&r.u.id).await;
    let queued = r.job(&r.u.id).await;
    let expired = r.job(&r.u.id).await;
    let fresh = r.job(&r.u.id).await;
    let barrier = r.job(&r.u.id).await;
    for job in [&refused, &queued] {
        Seed {
            refusal: Some("busy"),
            sent: true,
            age_secs: 5 * 60,
            ..Seed::new("refused")
        }
        .put(&r.u, job)
        .await;
    }
    Seed {
        sent: true,
        age_secs: 25 * 3600,
        ..Seed::new("expired")
    }
    .put(&r.u, &expired)
    .await;
    Seed {
        sent: true,
        ..Seed::new("requested")
    }
    .put(&r.u, &barrier)
    .await;
    let renewed = [&refused, &queued, &expired, &fresh];
    for job in renewed {
        r.object(job, "open").await;
    }
    let mut client = r.connect().await;
    let mut ids: Vec<&str> = renewed.iter().map(|j| j.as_str()).collect();
    ids.sort_unstable();
    let asked = request(&r.u, &alice, &ids).await;
    assert_eq!(asked.status, 202, "{}", asked.body);
    for job in renewed {
        assert_eq!(r.row(job).await, "requested|-|false|false");
    }
    // The answers to the commands of before the renewal, and an answer nobody asked for.
    answer(&mut client, &refused, "busy").await;
    answer(&mut client, &queued, "queued").await;
    answer(&mut client, &expired, "busy").await;
    answer(&mut client, &fresh, "noLog").await;
    // The answer to a sent row that comes after them has been applied when they all were.
    answer(&mut client, &barrier, "queued").await;
    r.until_row(&barrier, "accepted|-|true|true").await;
    for job in renewed {
        assert_eq!(r.row(job).await, "requested|-|false|false");
    }
    // Free the rows: each is sent and stamped, and then its answer applies.
    sqlx::query("UPDATE job_objects SET state = 'failed'")
        .execute(&r.u.w.pool)
        .await
        .unwrap();
    let mut sent = Vec::new();
    for _ in renewed {
        sent.push(command(&mut client).await.0);
    }
    sent.sort_unstable();
    assert_eq!(sent, ids);
    for job in renewed {
        r.until_row(job, "requested|-|true|false").await;
    }
    answer(&mut client, &refused, "busy").await;
    answer(&mut client, &queued, "queued").await;
    r.until_row(&refused, "refused|busy|true|true").await;
    r.until_row(&queued, "accepted|-|true|true").await;
    r.finish().await;
}

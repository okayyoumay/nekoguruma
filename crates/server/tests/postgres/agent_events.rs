//! The agent's outbox events at the server, against a throwaway PostgreSQL (design 5.3, 5.4;
//! ADR-478 item 6, ADR-479): fake agents are WebSocket clients of a server whose queue is on with
//! a short period, and the observable results are the rows of `agent_events`, the cursor, the
//! `eventAck` frames, the close codes and the queue's counters. The pure rule and the hub's seam
//! are unit tests in `src/agent_events.rs`, `src/hub.rs` and `src/job_queue.rs`; the real agent's
//! side is in `agent_connect.rs`.
//!
//! A case is named `E<n>` in a comment: E1 in order, E2 a repeat with the same text, E3 a repeat
//! with another text, E4 a repeat of a deleted row, E5 a gap beside a sibling, E6 a number beyond
//! `bigint`, E7 a repeat in one batch, E8 an acknowledgement after the commit, E9 a commit that is
//! refused, E10 a commit that went through though it was reported as failed, E11 a connection
//! replaced between the store and the acknowledgement, E12 a connection gone before the store, E13
//! two servers and a held lock, E14 no cursor row and concurrent first stores, E15 the cursor
//! created, E16 a revoked or missing agent, E17 tenants and another agent's job, E18 `received`
//! changes no job, E19 text, size and time of an event, E20 what is not stored, E21 the buffer
//! bound (unit tests only), E22 the ack slot (unit tests only), E23 a re-registration.
//!
//! The cases J5 to J16 and J20 (ADR-481) are what a stored `jobState` event does to the sender's
//! own job: J5 received, running and completed, J6 the failure columns, J7 a detail that holds
//! U+0000 and an oversized one, a procedure code too long and an unknown code, J8 a repeat, J9 a
//! `received` after a later state, J10 the same state twice, J11 a report after a completed job,
//! J12 an agent's needsOnSiteRecovery replaced, J13 another agent's, another tenant's, an unknown
//! and a non-canonical job, J14 a job the server ended, J15 `received` as the delivery's proof,
//! J16 a report and an answer to the command in one round, J20 a refused commit. J17 to J19 are
//! in `jobs.rs`, J21 in `agent_connect.rs`, J22 in `migrations.rs`.
//!
//! The tests run on the current-thread runtime and wait in real time, never on a paused clock; a
//! negative (a frame that must not come, a row that must not exist) is asserted only after
//! something positive that comes later in the same order has been seen. Every wait is bounded.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use server::Server;
use server::config::Http;
use server::job_queue::{EventCounts, Settings};
use shared_proto::{MAX_CONTROL_MESSAGE_LEN, ServerMessage};
use tokio_tungstenite::tungstenite::Message;

use crate::control::{Client, heartbeat, open, quiet_for, revoke_row, second_agent};
use crate::control::{text, token};
use crate::job_queue::{FRESH, ack, add_job, add_job_with, command, reject, row};
use crate::job_queue::{session_token, settings};
use crate::job_queue::{until_logged, until_row};
use crate::redeem::capture;
use crate::serving::Running;
use crate::session::Up;

const LIMIT: Duration = Duration::from_secs(15);
const JOB: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f";

/// A world whose queue counts the events it handles.
struct Rig {
    u: Up,
    counts: Arc<EventCounts>,
}

impl Rig {
    async fn new() -> Rig {
        Rig::with_lease(Duration::from_secs(600)).await
    }

    async fn with_lease(lease: Duration) -> Rig {
        let counts = Arc::new(EventCounts::default());
        let queue = Settings {
            events: Some(counts.clone()),
            ..settings(lease)
        };
        Rig {
            u: Up::with_queue(2, Some(queue)).await,
            counts,
        }
    }

    async fn connect(&self) -> Client {
        open(self.u.s.addr, &token(&self.u).await).await
    }

    async fn finish(self) {
        self.u.finish().await;
    }
}

/// The text of a `jobState` event for `job`; `extra` is more properties, each with its comma.
pub(crate) fn frame_of(seq: u64, job: &str, state: &str, extra: &str) -> String {
    format!(
        r#"{{"type":"agentEvent","seq":{seq},"kind":"jobState","jobId":"{job}","state":"{state}"{extra}}}"#
    )
}

fn frame(seq: u64) -> String {
    frame_of(seq, JOB, "running", "")
}

pub(crate) async fn send(client: &mut Client, body: &str) {
    client
        .send(Message::Text(body.to_owned().into()))
        .await
        .unwrap();
}

/// The next frame, which must be an `eventAck`; its number.
pub(crate) async fn read_ack(client: &mut Client) -> u64 {
    match ServerMessage::decode(&text(client).await).unwrap() {
        ServerMessage::EventAck { seq } => seq,
        other => panic!("an eventAck was due: {other:?}"),
    }
}

/// Reads acknowledgements until one reaches `seq`; none may pass it or go back.
async fn acked_up_to(client: &mut Client, seq: u64) {
    let mut last = 0;
    while last < seq {
        let got = read_ack(client).await;
        assert!(
            got > last && got <= seq,
            "ack {got} after {last}, up to {seq}"
        );
        last = got;
    }
}

/// Reads to the end of the connection: the frames of the server and the code of its Close.
async fn until_closed(client: &mut Client) -> (Vec<ServerMessage>, u16) {
    let mut frames = Vec::new();
    let mut code = None;
    loop {
        let next = tokio::time::timeout(LIMIT, client.next())
            .await
            .expect("the connection ends within the limit");
        match next {
            Some(Ok(Message::Text(text))) => {
                frames.push(ServerMessage::decode(text.as_str()).unwrap());
            }
            Some(Ok(Message::Close(Some(close)))) => code = Some(u16::from(close.code)),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => break,
        }
    }
    (frames, code.expect("a Close frame before the end"))
}

async fn until_count(counter: &AtomicU64, expected: u64, what: &str) {
    let deadline = Instant::now() + LIMIT;
    while counter.load(Ordering::Relaxed) != expected {
        assert!(
            Instant::now() < deadline,
            "{what} is {}, not {expected}",
            counter.load(Ordering::Relaxed)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn until_at_least(counter: &AtomicU64, expected: u64, what: &str) {
    let deadline = Instant::now() + LIMIT;
    while counter.load(Ordering::Relaxed) < expected {
        assert!(Instant::now() < deadline, "{what} did not reach {expected}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The rows of `agent`'s events: number and text.
async fn rows(u: &Up, agent: &str) -> Vec<(i64, String)> {
    sqlx::query_as("SELECT seq, body FROM agent_events WHERE agent_id = $1::uuid ORDER BY seq")
        .bind(agent)
        .fetch_all(&u.w.pool)
        .await
        .unwrap()
}

/// The agent's cursor, or -1 when it has no row.
async fn cursor(u: &Up, agent: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT coalesce((SELECT last_seq FROM agent_event_cursor WHERE agent_id = $1::uuid), -1)",
    )
    .bind(agent)
    .fetch_one(&u.w.pool)
    .await
    .unwrap()
}

/// Holds the agent's cursor row locked (the row is created first) until the transaction ends.
async fn hold_cursor(u: &Up, agent: &str) -> sqlx::Transaction<'static, sqlx::Postgres> {
    sqlx::query(
        "INSERT INTO agent_event_cursor (tenant_id, agent_id) \
         SELECT tenant_id, id FROM agents WHERE id = $1::uuid ON CONFLICT DO NOTHING",
    )
    .bind(agent)
    .execute(&u.w.pool)
    .await
    .unwrap();
    let mut tx = u.w.pool.begin().await.unwrap();
    sqlx::query("SELECT last_seq FROM agent_event_cursor WHERE agent_id = $1::uuid FOR UPDATE")
        .bind(agent)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx
}

/// Makes the commit of a transaction that inserts an event fail, as a lost connection would.
async fn refuse_events(u: &Up) {
    u.w.pg
        .execute(
            "CREATE OR REPLACE FUNCTION test_commit_refused() RETURNS trigger LANGUAGE plpgsql \
             AS $$ BEGIN RAISE EXCEPTION 'test: commit refused'; END $$",
        )
        .await;
    u.w.pg
        .execute(
            "CREATE CONSTRAINT TRIGGER test_events_refused AFTER INSERT ON agent_events \
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION test_commit_refused()",
        )
        .await;
}

// E3 (a later round)
#[tokio::test]
async fn a_connection_closed_for_a_differing_repeat_has_its_later_events_dropped_in_a_later_round()
{
    let r = Rig::new().await;
    let (other_id, other_token) = second_agent(&r.u).await;
    let mut other = open(r.u.s.addr, &other_token).await;
    let mut client = r.connect().await;
    for seq in 1..=2 {
        send(&mut client, &frame(seq)).await;
    }
    acked_up_to(&mut client, 2).await;
    let held = hold_cursor(&r.u, &r.u.id).await;
    // A differing 2 waits on the lock in its round; the 3 is read meanwhile and is for the next.
    send(&mut client, &frame_of(2, JOB, "failed", "")).await;
    until_count(&r.counts.seen, 3, "the events seen").await;
    send(&mut client, &frame(3)).await;
    // Time for the connection to read the 3 into the inbox before its close can come.
    tokio::time::sleep(Duration::from_millis(300)).await;
    held.commit().await.unwrap();
    assert_eq!(until_closed(&mut client).await.1, 1002);
    until_count(&r.counts.seen, 4, "the events seen").await;
    // Something positive that is handled after the dropped 3.
    send(&mut other, &frame(1)).await;
    assert_eq!(read_ack(&mut other).await, 1);
    assert_eq!(rows(&r.u, &r.u.id).await, [(1, frame(1)), (2, frame(2))]);
    assert_eq!(cursor(&r.u, &r.u.id).await, 2);
    assert_eq!(rows(&r.u, &other_id).await.len(), 1);
    r.finish().await;
}

// E9 (one failing agent beside another)
#[tokio::test]
async fn an_agent_whose_store_keeps_failing_does_not_keep_another_agent_from_being_stored() {
    let (log, _guard) = capture();
    let r = Rig::new().await;
    let (other_id, other_token) = second_agent(&r.u).await;
    r.u.w
        .pg
        .execute(
            "CREATE FUNCTION test_x_refused() RETURNS trigger LANGUAGE plpgsql \
             AS $$ BEGIN RAISE EXCEPTION 'test: insert refused'; END $$",
        )
        .await;
    // The agent's ID is a UUID the server generated: nothing a test input controls.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER test_x_refused BEFORE INSERT ON agent_events FOR EACH ROW \
         WHEN (NEW.agent_id = '{}'::uuid) EXECUTE FUNCTION test_x_refused()",
        r.u.id
    )))
    .execute(&r.u.w.pool)
    .await
    .unwrap();
    let mut x = r.connect().await;
    let mut y = open(r.u.s.addr, &other_token).await;
    send(&mut x, &frame(1)).await;
    until_logged(&log, "could not store events").await;
    send(&mut y, &frame(1)).await;
    assert_eq!(read_ack(&mut y).await, 1);
    assert_eq!(rows(&r.u, &other_id).await.len(), 1);
    assert!(rows(&r.u, &r.u.id).await.is_empty());
    // The failing agent is retried and still not stored; its events were kept.
    send(&mut y, &frame(2)).await;
    assert_eq!(read_ack(&mut y).await, 2);
    quiet_for(&mut x, 300).await;
    assert!(rows(&r.u, &r.u.id).await.is_empty());
    r.u.w
        .pg
        .execute("DROP TRIGGER test_x_refused ON agent_events")
        .await;
    assert_eq!(read_ack(&mut x).await, 1);
    r.finish().await;
}

// E1
#[tokio::test]
async fn events_in_order_are_stored_once_each_with_their_exact_text_and_acknowledged_up_to_the_last()
 {
    let r = Rig::new().await;
    let mut client = r.connect().await;
    let texts: Vec<String> = (1..=3).map(frame).collect();
    for body in &texts {
        send(&mut client, body).await;
    }
    acked_up_to(&mut client, 3).await;
    let stored = rows(&r.u, &r.u.id).await;
    let expected: Vec<(i64, String)> = (1..).zip(texts).collect();
    assert_eq!(stored, expected, "the frame's text, byte for byte");
    assert_eq!(cursor(&r.u, &r.u.id).await, 3);
    let derived: String = r
        .u
        .w
        .sql(&format!(
            "SELECT string_agg(DISTINCT kind || '|' || job_id::text || '|' || \
                    (tenant_id = (SELECT tenant_id FROM agents WHERE id = '{}'))::text || '|' || \
                    (occurred_at_agent IS NULL)::text || '|' || \
                    (received_at <= now() AND received_at > now() - interval '1 minute')::text, ',') \
             FROM agent_events",
            r.u.id
        ))
        .await;
    assert_eq!(derived, format!("jobState|{JOB}|true|true|true"));
    assert_eq!(r.counts.stored.load(Ordering::Relaxed), 3);
    assert_eq!(r.counts.duplicates.load(Ordering::Relaxed), 0);
    r.finish().await;
}

// E2
#[tokio::test]
async fn a_repeat_with_the_same_text_is_not_stored_and_is_answered_with_the_cursor() {
    let r = Rig::new().await;
    let mut client = r.connect().await;
    for seq in 1..=3 {
        send(&mut client, &frame(seq)).await;
    }
    acked_up_to(&mut client, 3).await;
    send(&mut client, &frame(2)).await;
    assert_eq!(
        read_ack(&mut client).await,
        3,
        "the cursor, not the repeated number"
    );
    assert_eq!(rows(&r.u, &r.u.id).await.len(), 3);
    assert_eq!(cursor(&r.u, &r.u.id).await, 3);
    assert_eq!(r.counts.duplicates.load(Ordering::Relaxed), 1);
    assert_eq!(r.counts.stored.load(Ordering::Relaxed), 3);
    r.finish().await;
}

// E3
#[tokio::test]
async fn a_repeat_with_another_text_stores_and_acknowledges_nothing_and_closes_that_connection_only()
 {
    let (log, _guard) = capture();
    let r = Rig::new().await;
    let (other_id, other_token) = second_agent(&r.u).await;
    let mut client = r.connect().await;
    let mut other = open(r.u.s.addr, &other_token).await;
    for seq in 1..=2 {
        send(&mut client, &frame(seq)).await;
    }
    acked_up_to(&mut client, 2).await;
    let secret = "a-detail-that-must-not-be-logged";
    send(
        &mut client,
        &frame_of(1, JOB, "failed", &format!(r#","detail":"{secret}""#)),
    )
    .await;
    let (frames, code) = until_closed(&mut client).await;
    assert_eq!(code, 1002);
    assert!(frames.is_empty(), "no acknowledgement: {frames:?}");
    assert_eq!(
        rows(&r.u, &r.u.id).await,
        [(1, frame(1)), (2, frame(2))],
        "the stored text stays"
    );
    assert_eq!(cursor(&r.u, &r.u.id).await, 2);
    assert_eq!(r.counts.duplicates.load(Ordering::Relaxed), 0);
    // The warning names the agent and the number, never the text.
    until_logged(&log, "differs from the stored one").await;
    let logged = log.text();
    assert!(
        logged.contains(&r.u.id) && logged.contains("seq=1"),
        "{logged}"
    );
    assert!(!logged.contains(secret), "the text is never logged");
    // The other agent's connection is untouched and stores on.
    send(&mut other, &frame(1)).await;
    assert_eq!(read_ack(&mut other).await, 1);
    heartbeat(&mut other).await;
    assert_eq!(rows(&r.u, &other_id).await.len(), 1);
    // The agent's next connection sends its first pending event again and goes on.
    let mut again = r.connect().await;
    send(&mut again, &frame(2)).await;
    assert_eq!(read_ack(&mut again).await, 2);
    send(&mut again, &frame(3)).await;
    assert_eq!(read_ack(&mut again).await, 3);
    r.finish().await;
}

// E4
#[tokio::test]
async fn a_repeat_of_a_deleted_row_is_answered_with_the_cursor_and_the_cursor_goes_on() {
    let r = Rig::new().await;
    let mut client = r.connect().await;
    for seq in 1..=3 {
        send(&mut client, &frame(seq)).await;
    }
    acked_up_to(&mut client, 3).await;
    sqlx::query("DELETE FROM agent_events WHERE agent_id = $1::uuid AND seq < 3")
        .bind(&r.u.id)
        .execute(&r.u.w.pool)
        .await
        .unwrap();
    // The text of a deleted row cannot be compared: a repeat of either text is answered alike.
    send(&mut client, &frame(2)).await;
    assert_eq!(read_ack(&mut client).await, 3);
    send(&mut client, &frame_of(1, JOB, "failed", "")).await;
    assert_eq!(read_ack(&mut client).await, 3);
    assert_eq!(
        rows(&r.u, &r.u.id).await,
        [(3, frame(3))],
        "nothing came back"
    );
    assert_eq!(
        cursor(&r.u, &r.u.id).await,
        3,
        "deleting rows left the cursor"
    );
    send(&mut client, &frame(4)).await;
    assert_eq!(read_ack(&mut client).await, 4);
    assert_eq!(r.counts.duplicates.load(Ordering::Relaxed), 2);
    r.finish().await;
}

// E5
#[tokio::test]
async fn a_gap_is_neither_stored_nor_acknowledged_and_closes_its_connection_beside_a_sibling() {
    let r = Rig::new().await;
    let (other_id, other_token) = second_agent(&r.u).await;
    let mut client = r.connect().await;
    let mut other = open(r.u.s.addr, &other_token).await;
    for seq in 1..=2 {
        send(&mut client, &frame(seq)).await;
    }
    acked_up_to(&mut client, 2).await;
    send(&mut client, &frame(4)).await;
    let (frames, code) = until_closed(&mut client).await;
    assert_eq!(code, 1002);
    assert!(frames.is_empty(), "the gap is not acknowledged: {frames:?}");
    assert_eq!(rows(&r.u, &r.u.id).await.len(), 2);
    assert_eq!(cursor(&r.u, &r.u.id).await, 2);
    // The sibling agent is served in the same queue and its connection lives.
    send(&mut other, &frame(1)).await;
    assert_eq!(read_ack(&mut other).await, 1);
    assert_eq!(rows(&r.u, &other_id).await.len(), 1);
    // A new connection sends from the first pending event, and the number that was skipped fits.
    let mut again = r.connect().await;
    send(&mut again, &frame(3)).await;
    send(&mut again, &frame(4)).await;
    acked_up_to(&mut again, 4).await;
    assert_eq!(cursor(&r.u, &r.u.id).await, 4);
    r.finish().await;
}

// E6
#[tokio::test]
async fn a_number_no_bigint_holds_closes_the_connection_and_the_largest_one_that_fits_is_a_repeat()
{
    let r = Rig::new().await;
    let mut client = r.connect().await;
    send(&mut client, &frame(9_223_372_036_854_775_808)).await;
    let (frames, code) = until_closed(&mut client).await;
    assert_eq!((frames.len(), code), (0, 1002));
    send_to_new(&r, &frame(u64::MAX - 1)).await;
    assert!(rows(&r.u, &r.u.id).await.is_empty());
    // A cursor at the top of the range: the number itself is a repeat of a row that is not there,
    // and the next one is beyond what the column holds.
    sqlx::query(
        "INSERT INTO agent_event_cursor (tenant_id, agent_id, last_seq) \
         SELECT tenant_id, id, 9223372036854775807 FROM agents WHERE id = $1::uuid \
         ON CONFLICT (tenant_id, agent_id) DO UPDATE SET last_seq = 9223372036854775807",
    )
    .bind(&r.u.id)
    .execute(&r.u.w.pool)
    .await
    .unwrap();
    let mut client = r.connect().await;
    send(&mut client, &frame(9_223_372_036_854_775_807)).await;
    assert_eq!(read_ack(&mut client).await, 9_223_372_036_854_775_807);
    send(&mut client, &frame(9_223_372_036_854_775_808)).await;
    let (frames, code) = until_closed(&mut client).await;
    assert_eq!((frames.len(), code), (0, 1002));
    assert!(rows(&r.u, &r.u.id).await.is_empty());
    assert_eq!(cursor(&r.u, &r.u.id).await, i64::MAX);
    r.finish().await;
}

/// Sends `body` on a new connection, which the server must close with 1002.
async fn send_to_new(r: &Rig, body: &str) {
    let mut client = r.connect().await;
    send(&mut client, body).await;
    assert_eq!(until_closed(&mut client).await.1, 1002);
}

// E7
#[tokio::test]
async fn a_repeat_in_the_same_batch_is_a_repeat_of_the_row_stored_just_before_it() {
    let r = Rig::new().await;
    let mut client = r.connect().await;
    send(&mut client, &frame(1)).await;
    send(&mut client, &frame(1)).await;
    send(&mut client, &frame(2)).await;
    acked_up_to(&mut client, 2).await;
    assert_eq!(rows(&r.u, &r.u.id).await.len(), 2);
    assert_eq!(r.counts.stored.load(Ordering::Relaxed), 2);
    assert_eq!(r.counts.duplicates.load(Ordering::Relaxed), 1);
    // The same, with a second text for the number stored just before: the stored one stays.
    send(&mut client, &frame(3)).await;
    send(&mut client, &frame_of(3, JOB, "failed", "")).await;
    let (_, code) = until_closed(&mut client).await;
    assert_eq!(code, 1002);
    assert_eq!(rows(&r.u, &r.u.id).await.last().unwrap(), &(3, frame(3)));
    r.finish().await;
}

// E8
#[tokio::test]
async fn an_acknowledgement_is_never_read_before_its_row_and_cursor_are_visible() {
    let r = Rig::new().await;
    let mut client = r.connect().await;
    for seq in 1..=25 {
        send(&mut client, &frame(seq)).await;
        let acked = read_ack(&mut client).await;
        assert_eq!(acked, seq);
        // Read at once, from another connection of the database, with no wait.
        assert_eq!(cursor(&r.u, &r.u.id).await, i64::try_from(seq).unwrap());
        assert_eq!(
            rows(&r.u, &r.u.id).await.len(),
            usize::try_from(seq).unwrap()
        );
    }
    r.finish().await;
}

// E9
#[tokio::test]
async fn a_commit_that_is_refused_stores_and_acknowledges_nothing_and_the_events_are_stored_later()
{
    let (log, _guard) = capture();
    let r = Rig::new().await;
    refuse_events(&r.u).await;
    let mut client = r.connect().await;
    send(&mut client, &frame(1)).await;
    send(&mut client, &frame(2)).await;
    until_logged(&log, "could not store events").await;
    assert!(rows(&r.u, &r.u.id).await.is_empty());
    assert_eq!(
        cursor(&r.u, &r.u.id).await,
        -1,
        "the cursor row went with the transaction"
    );
    quiet_for(&mut client, 300).await;
    assert_eq!(r.counts.stored.load(Ordering::Relaxed), 0);
    // The queue kept the events and tries again on its tick.
    r.u.w
        .pg
        .execute("DROP TRIGGER test_events_refused ON agent_events")
        .await;
    acked_up_to(&mut client, 2).await;
    assert_eq!(rows(&r.u, &r.u.id).await.len(), 2);
    assert_eq!(cursor(&r.u, &r.u.id).await, 2);
    assert_eq!(r.counts.stored.load(Ordering::Relaxed), 2);
    assert_eq!(
        r.counts.seen.load(Ordering::Relaxed),
        2,
        "each event is seen once"
    );
    r.finish().await;
}

// E10: a commit that went through and was reported as failed cannot be made to happen here, so
// the state it leaves is staged: the row and the cursor are in, and the queue still holds the event.
#[tokio::test]
async fn an_event_whose_commit_went_through_though_reported_as_failed_is_a_repeat_when_sent_again()
{
    let (log, _guard) = capture();
    let r = Rig::new().await;
    refuse_events(&r.u).await;
    let mut client = r.connect().await;
    send(&mut client, &frame(1)).await;
    until_logged(&log, "could not store events").await;
    // Staged: what the commit would have left (the triggers do not fire in the replica role).
    let mut tx = r.u.w.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agent_events (tenant_id, agent_id, seq, kind, job_id, body) \
         SELECT tenant_id, id, 1, 'jobState', $2::uuid, $3 FROM agents WHERE id = $1::uuid",
    )
    .bind(&r.u.id)
    .bind(JOB)
    .bind(frame(1))
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_event_cursor (tenant_id, agent_id, last_seq) \
         SELECT tenant_id, id, 1 FROM agents WHERE id = $1::uuid",
    )
    .bind(&r.u.id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    // The queue's next attempt finds a repeat with the same text: acknowledged, nothing doubled.
    assert_eq!(read_ack(&mut client).await, 1);
    assert_eq!(rows(&r.u, &r.u.id).await.len(), 1);
    assert_eq!(cursor(&r.u, &r.u.id).await, 1);
    assert_eq!(r.counts.stored.load(Ordering::Relaxed), 0);
    assert_eq!(r.counts.duplicates.load(Ordering::Relaxed), 1);
    r.finish().await;
}

// E11
#[tokio::test]
async fn a_connection_replaced_between_the_store_and_the_acknowledgement_is_not_acknowledged_and_its_successor_gets_nothing()
 {
    let r = Rig::new().await;
    let mut old = r.connect().await;
    let held = hold_cursor(&r.u, &r.u.id).await;
    send(&mut old, &frame(1)).await;
    until_count(&r.counts.seen, 1, "the events seen").await;
    // The store waits on the lock; the agent connects again and replaces the connection.
    let mut new = r.connect().await;
    assert_eq!(until_closed(&mut old).await.1, 4001, "replaced");
    held.commit().await.unwrap();
    until_count(&r.counts.stored, 1, "the events stored").await;
    assert_eq!(rows(&r.u, &r.u.id).await.len(), 1);
    quiet_for(&mut new, 300).await;
    // The agent's resend on the new connection is a repeat, answered on that connection.
    send(&mut new, &frame(1)).await;
    assert_eq!(read_ack(&mut new).await, 1);
    r.finish().await;
}

// E12
#[tokio::test]
async fn an_event_of_a_connection_gone_before_the_store_is_stored_without_an_acknowledgement() {
    let r = Rig::new().await;
    let mut client = r.connect().await;
    let held = hold_cursor(&r.u, &r.u.id).await;
    send(&mut client, &frame(1)).await;
    until_count(&r.counts.seen, 1, "the events seen").await;
    client.close(None).await.unwrap();
    drop(client);
    let deadline = Instant::now() + LIMIT;
    while !r.u.hub.connected().is_empty() {
        assert!(Instant::now() < deadline, "the connection did not leave");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    held.commit().await.unwrap();
    until_count(&r.counts.stored, 1, "the events stored").await;
    assert_eq!(cursor(&r.u, &r.u.id).await, 1);
    // The agent connects again and sends the event again: a repeat.
    let mut again = r.connect().await;
    send(&mut again, &frame(1)).await;
    assert_eq!(read_ack(&mut again).await, 1);
    r.finish().await;
}

// E13
#[tokio::test]
async fn two_servers_serve_one_agent_in_turn_on_the_held_cursor_and_store_each_event_once() {
    let first = Arc::new(EventCounts::default());
    let second = Arc::new(EventCounts::default());
    let u = Up::with_queue(
        2,
        Some(Settings {
            events: Some(first.clone()),
            ..settings(Duration::from_secs(600))
        }),
    )
    .await;
    let http = Http {
        port: 0,
        shutdown_grace_secs: 2,
    };
    let s2 = Running::spawn(
        Server::bind(&http, u.w.pg.pool(), u.w.key.clone())
            .await
            .unwrap()
            .with_job_queue(Settings {
                events: Some(second.clone()),
                ..settings(Duration::from_secs(600))
            }),
    );
    let t2 = session_token(u.valid_on(&s2).await).await;
    let mut c1 = open(u.s.addr, &token(&u).await).await;
    let mut c2 = open(s2.addr, &t2).await;
    let held = hold_cursor(&u, &u.id).await;
    for client in [&mut c1, &mut c2] {
        send(client, &frame(1)).await;
        send(client, &frame(2)).await;
    }
    // Each server has read the first event and waits on the lock in turn (the second one is read
    // after that store ends); the lock is released well before the 5 s lock timeout, so the wait
    // itself is what is tested, not the failure and the retry.
    until_at_least(&first.seen, 1, "the first server's events seen").await;
    until_at_least(&second.seen, 1, "the second server's events seen").await;
    let released = Instant::now();
    held.commit().await.unwrap();
    acked_up_to(&mut c1, 2).await;
    acked_up_to(&mut c2, 2).await;
    assert!(
        released.elapsed() < Duration::from_secs(4),
        "the stores waited in turn, they did not fail on the lock timeout"
    );
    assert_eq!(rows(&u, &u.id).await.len(), 2);
    assert_eq!(cursor(&u, &u.id).await, 2);
    let stored = first.stored.load(Ordering::Relaxed) + second.stored.load(Ordering::Relaxed);
    let repeats =
        first.duplicates.load(Ordering::Relaxed) + second.duplicates.load(Ordering::Relaxed);
    assert_eq!(
        (stored, repeats),
        (2, 2),
        "each event once, its copy a repeat"
    );
    s2.finish().await;
    u.finish().await;
}

// E14
#[tokio::test]
async fn a_first_store_waits_for_a_concurrent_creation_of_the_cursor_row_and_fails_on_nothing() {
    let (log, _guard) = capture();
    let r = Rig::new().await;
    let mut client = r.connect().await;
    // Another server's first store: the cursor row is inserted and not yet committed.
    let mut tx = r.u.w.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO agent_event_cursor (tenant_id, agent_id) \
         SELECT tenant_id, id FROM agents WHERE id = $1::uuid",
    )
    .bind(&r.u.id)
    .execute(&mut *tx)
    .await
    .unwrap();
    send(&mut client, &frame(1)).await;
    until_count(&r.counts.seen, 1, "the events seen").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(r.counts.stored.load(Ordering::Relaxed), 0, "it waits");
    tx.commit().await.unwrap();
    assert_eq!(read_ack(&mut client).await, 1);
    assert_eq!(cursor(&r.u, &r.u.id).await, 1);
    assert_eq!(rows(&r.u, &r.u.id).await.len(), 1);
    assert!(
        !log.text().contains("could not store events"),
        "{}",
        log.text()
    );
    r.finish().await;
}

// E15
#[tokio::test]
async fn the_first_stored_event_creates_the_cursor_row_of_the_agents_tenant() {
    let r = Rig::new().await;
    assert_eq!(cursor(&r.u, &r.u.id).await, -1, "no row before");
    let mut client = r.connect().await;
    send(&mut client, &frame(1)).await;
    assert_eq!(read_ack(&mut client).await, 1);
    let line: String =
        r.u.w
            .sql(&format!(
                "SELECT c.last_seq || '|' || (c.tenant_id = a.tenant_id)::text \
             FROM agent_event_cursor c JOIN agents a ON a.id = c.agent_id WHERE a.id = '{}'",
                r.u.id
            ))
            .await;
    assert_eq!(line, "1|true");
    r.finish().await;
}

// E16
#[tokio::test]
async fn nothing_is_stored_or_answered_for_a_revoked_or_missing_agent_and_others_go_on() {
    let r = Rig::new().await;
    let (missing_id, missing_token) = second_agent(&r.u).await;
    let (live_id, live_token) = second_agent(&r.u).await;
    let mut revoked = r.connect().await;
    let mut missing = open(r.u.s.addr, &missing_token).await;
    let mut live = open(r.u.s.addr, &live_token).await;
    revoke_row(&r.u, &r.u.id).await;
    let mut tx = r.u.w.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM agents WHERE id = $1::uuid")
        .bind(&missing_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    send(&mut revoked, &frame(1)).await;
    send(&mut missing, &frame(1)).await;
    until_count(&r.counts.seen, 2, "the events seen").await;
    // The live agent's event comes after both in the inbox, so the queue is past them once it is
    // acknowledged.
    send(&mut live, &frame(1)).await;
    assert_eq!(read_ack(&mut live).await, 1);
    assert!(rows(&r.u, &r.u.id).await.is_empty());
    assert!(rows(&r.u, &missing_id).await.is_empty());
    assert_eq!(rows(&r.u, &live_id).await.len(), 1);
    assert_eq!(cursor(&r.u, &r.u.id).await, -1);
    assert_eq!(cursor(&r.u, &missing_id).await, -1);
    quiet_for(&mut revoked, 200).await;
    quiet_for(&mut missing, 200).await;
    r.finish().await;
}

// E17
#[tokio::test]
async fn an_event_is_stored_under_its_senders_tenant_whatever_job_it_names_and_changes_no_job() {
    let r = Rig::new().await;
    let (other_id, other_token) = second_agent(&r.u).await;
    // The second agent works for another tenant.
    sqlx::query(
        "WITH t AS (INSERT INTO tenants (id, name) VALUES (gen_random_uuid(), 'other') RETURNING id) \
         UPDATE agents SET tenant_id = (SELECT id FROM t) WHERE id = $1::uuid",
    )
    .bind(&other_id)
    .execute(&r.u.w.pool)
    .await
    .unwrap();
    // A job of the second agent's, which the first agent's event names; the agent is not
    // connected, so nothing delivers it.
    let (third_id, _) = second_agent(&r.u).await;
    let job = add_job(&r.u.w, &third_id, 1, b"x").await;
    let mut client = r.connect().await;
    let mut other = open(r.u.s.addr, &other_token).await;
    send(&mut client, &frame_of(1, &job, "running", "")).await;
    send(&mut other, &frame_of(1, &job, "failed", "")).await;
    assert_eq!(read_ack(&mut client).await, 1);
    assert_eq!(read_ack(&mut other).await, 1);
    let line: String = r
        .u
        .w
        .sql(&format!(
            "SELECT string_agg(e.agent_id::text || '|' || (e.tenant_id = a.tenant_id)::text || '|' \
                    || (e.job_id = '{job}')::text, ',' ORDER BY e.agent_id) \
             FROM agent_events e JOIN agents a ON a.id = e.agent_id"
        ))
        .await;
    let mut expected = [
        format!("{}|true|true", r.u.id),
        format!("{other_id}|true|true"),
    ];
    expected.sort();
    assert_eq!(line, expected.join(","));
    let tenants: String =
        r.u.w
            .sql("SELECT count(DISTINCT tenant_id) FROM agent_event_cursor")
            .await;
    assert_eq!(tenants, "2");
    assert_eq!(row(&r.u.w, &job).await, FRESH, "the job is as it was");
    r.finish().await;
}

// E18
#[tokio::test]
async fn a_received_event_changes_no_job_row() {
    let r = Rig::new().await;
    let (idle_id, _) = second_agent(&r.u).await;
    let job = add_job(&r.u.w, &idle_id, 1, b"envelope").await;
    let before = row(&r.u.w, &job).await;
    let mut client = r.connect().await;
    send(&mut client, &frame_of(1, &job, "received", "")).await;
    assert_eq!(read_ack(&mut client).await, 1);
    assert_eq!(row(&r.u.w, &job).await, before);
    r.finish().await;
}

// E19
#[tokio::test]
async fn the_text_of_an_event_is_kept_as_sent_at_its_largest_and_a_device_time_only_when_it_is_strict()
 {
    let r = Rig::new().await;
    let mut client = r.connect().await;
    // A detail with the escape for U+0000 (which `jsonb` would refuse), a frame of exactly the
    // limit, and a device time that is not a date.
    let nul = frame_of(1, JOB, "failed", r#","detail":"a\u0000b""#);
    let base = frame_of(2, JOB, "failed", r#","detail":"""#);
    let big = frame_of(
        2,
        JOB,
        "failed",
        &format!(
            r#","detail":"{}""#,
            "d".repeat(MAX_CONTROL_MESSAGE_LEN - base.len())
        ),
    );
    assert_eq!(big.len(), MAX_CONTROL_MESSAGE_LEN);
    let bad_time = frame_of(
        3,
        JOB,
        "running",
        r#","occurredAtAgent":"2026-02-30T10:00:00Z""#,
    );
    let good_time = frame_of(
        4,
        JOB,
        "running",
        r#","occurredAtAgent":"2026-11-14T10:20:30.25+09:00""#,
    );
    for body in [&nul, &big, &bad_time, &good_time] {
        send(&mut client, body).await;
    }
    acked_up_to(&mut client, 4).await;
    let stored = rows(&r.u, &r.u.id).await;
    assert_eq!(stored[0].1, nul);
    assert!(stored[0].1.contains(r"\u0000"));
    assert_eq!(stored[1].1, big, "128 KiB, byte for byte");
    let times: String =
        r.u.w
            .sql(
                "SELECT string_agg((occurred_at_agent IS NULL)::text, ',' ORDER BY seq) \
             FROM agent_events",
            )
            .await;
    assert_eq!(times, "true,true,true,false");
    assert_eq!(
        r.u.w
            .sql("SELECT occurred_at_agent = '2026-11-14T01:20:30.25Z'::timestamptz FROM agent_events WHERE seq = 4")
            .await,
        "true"
    );
    // The edges the strict check accepts are all times the database reads.
    let edges = [
        "0001-01-01T00:00:00Z",
        "9999-12-31T23:59:59.999999999Z",
        "2024-02-29T12:00:00-15:59",
        "2026-11-14T10:20:30+15:59",
    ];
    for (n, edge) in edges.iter().enumerate() {
        let seq = 5 + u64::try_from(n).unwrap();
        send(
            &mut client,
            &frame_of(
                seq,
                JOB,
                "running",
                &format!(r#","occurredAtAgent":"{edge}""#),
            ),
        )
        .await;
    }
    acked_up_to(&mut client, 8).await;
    let kept: String = r
        .u
        .w
        .sql("SELECT count(*) FROM agent_events WHERE seq >= 5 AND occurred_at_agent IS NOT NULL")
        .await;
    assert_eq!(kept, "4");
    r.finish().await;
}

// E20
#[tokio::test]
async fn progress_and_reconcile_messages_are_not_stored_and_do_not_count_as_events() {
    let r = Rig::new().await;
    let mut client = r.connect().await;
    send(
        &mut client,
        &format!(
            r#"{{"type":"agentEvent","kind":"progress","jobId":"{JOB}","progressPermille":5,"phase":"p"}}"#
        ),
    )
    .await;
    send(&mut client, r#"{"type":"reconcile","openJobs":[]}"#).await;
    send(&mut client, &frame(1)).await;
    assert_eq!(read_ack(&mut client).await, 1);
    assert_eq!(rows(&r.u, &r.u.id).await.len(), 1);
    assert_eq!(r.counts.seen.load(Ordering::Relaxed), 1);
    heartbeat(&mut client).await;
    r.finish().await;
}

// E23
#[tokio::test]
async fn a_re_registered_agent_has_its_own_cursor_and_starts_at_one() {
    let r = Rig::new().await;
    let mut old = r.connect().await;
    for seq in 1..=3 {
        send(&mut old, &frame(seq)).await;
    }
    acked_up_to(&mut old, 3).await;
    // The same device registers again: a new agent ID, a new outbox that numbers from 1.
    let (new_id, new_token) = second_agent(&r.u).await;
    let mut new = open(r.u.s.addr, &new_token).await;
    send(&mut new, &frame(1)).await;
    assert_eq!(read_ack(&mut new).await, 1);
    assert_eq!(rows(&r.u, &new_id).await.len(), 1);
    assert_eq!(cursor(&r.u, &new_id).await, 1);
    assert_eq!(
        cursor(&r.u, &r.u.id).await,
        3,
        "the old agent's cursor is its own"
    );
    r.finish().await;
}

// ---------------------------------------------------------------------------------------------
// What a stored `jobState` event does to the sender's own job (design 5.6, ADR-481)

/// A job of `agent` that no delivery will touch: its mark is outstanding (the lease of a rig is
/// long) and, when `delivered`, the agent has acknowledged it. Its state is `received`.
async fn own_job(u: &Up, agent: &str, delivered: bool) -> String {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, id, kind, agent_id, operator_id, vci_id, \
             confirmation_level, start_deadline, signed_job, signing_key_id, idempotency_key, \
             delivery_attempts, delivery_sent_at, delivered_at) \
         SELECT a.tenant_id, gen_random_uuid(), 'runSequence', a.id, o.id, 'vci', 'notifyOnly', \
             now() + interval '1 hour', 'x', $2, gen_random_uuid()::text, 1, now(), \
             CASE WHEN $3 THEN now() END \
         FROM agents a JOIN operators o ON o.tenant_id = a.tenant_id WHERE a.id = $1::uuid \
         RETURNING id::text",
    )
    .bind(agent)
    .bind(u.w.key.key_id())
    .bind(delivered)
    .fetch_one(&u.w.pool)
    .await
    .unwrap()
}

/// The job's state, failure, end and stamps as one line: state, failure code, procedure code,
/// detail, then whether it is finished, was reported on and was delivered (`-` for none).
async fn line(u: &Up, job: &str) -> String {
    sqlx::query_scalar(
        "SELECT concat_ws('|', state, coalesce(failure_code, '-'), coalesce(procedure_code, '-'), \
             coalesce(failure_detail, '-'), (finished_at IS NOT NULL)::text, \
             (last_reported_at IS NOT NULL)::text, (delivered_at IS NOT NULL)::text) \
         FROM jobs WHERE id = $1::uuid",
    )
    .bind(job)
    .fetch_one(&u.w.pool)
    .await
    .unwrap()
}

/// A timestamp column of the job, as text (null reads `-`).
async fn stamp(u: &Up, job: &str, column: &str) -> String {
    u.w.sql(&format!(
        "SELECT coalesce({column}::text, '-') FROM jobs WHERE id = '{job}'"
    ))
    .await
}

async fn is_later(u: &Up, later: &str, earlier: &str) -> bool {
    u.w.sql(&format!(
        "SELECT '{later}'::timestamptz > '{earlier}'::timestamptz"
    ))
    .await
        == "true"
}

/// Sends the event and waits for its acknowledgement, so the job is as the commit left it.
pub(crate) async fn say(client: &mut Client, seq: u64, job: &str, state: &str, extra: &str) {
    send(client, &frame_of(seq, job, state, extra)).await;
    assert_eq!(read_ack(client).await, seq);
}

// J7: written first. The detail holds the escape for U+0000, which a `text` column refuses; if
// the server wrote it as it came, the agent's transaction would fail again on every retry.
#[tokio::test]
async fn a_detail_with_nul_is_kept_with_a_replacement_cut_to_1024_bytes_and_a_bad_code_is_other() {
    let r = Rig::new().await;
    let a = own_job(&r.u, &r.u.id, true).await;
    let b = own_job(&r.u, &r.u.id, true).await;
    let c = own_job(&r.u, &r.u.id, true).await;
    let mut client = r.connect().await;
    let detail = format!("a\\u0000b{}", "d".repeat(2000));
    let long_code = "p".repeat(129);
    say(
        &mut client,
        1,
        &a,
        "failed",
        &format!(r#","failureCode":"procedure","procedureCode":"{long_code}","detail":"{detail}""#),
    )
    .await;
    say(
        &mut client,
        2,
        &b,
        "failed",
        r#","failureCode":"bogusCode""#,
    )
    .await;
    say(&mut client, 3, &c, "running", "").await;
    let kept: String =
        r.u.w
            .sql(&format!("SELECT failure_detail FROM jobs WHERE id = '{a}'"))
            .await;
    assert_eq!(kept, format!("a\u{fffd}b{}", "d".repeat(1019)));
    assert_eq!(kept.len(), 1024);
    assert_eq!(line(&r.u, &a).await[..16], *"failed|procedure");
    assert_eq!(
        line(&r.u, &a).await.split('|').nth(2),
        Some("-"),
        "a procedure code of 129 bytes is left out, not cut"
    );
    assert_eq!(line(&r.u, &b).await, "failed|other|-|-|true|true|true");
    assert_eq!(line(&r.u, &c).await, "running|-|-|-|false|true|true");
    assert_eq!(rows(&r.u, &r.u.id).await.len(), 3);
    assert_eq!(cursor(&r.u, &r.u.id).await, 3);
    assert_eq!(r.counts.applied.load(Ordering::Relaxed), 3);
    r.finish().await;
}

// J5
#[tokio::test]
async fn received_running_and_completed_move_the_senders_own_job() {
    let r = Rig::new().await;
    let job = own_job(&r.u, &r.u.id, false).await;
    let mut client = r.connect().await;
    assert_eq!(line(&r.u, &job).await, "received|-|-|-|false|false|false");
    // `received` stamps the report and the delivery and never moves the state.
    say(&mut client, 1, &job, "received", "").await;
    assert_eq!(line(&r.u, &job).await, "received|-|-|-|false|true|true");
    say(&mut client, 2, &job, "running", "").await;
    assert_eq!(line(&r.u, &job).await, "running|-|-|-|false|true|true");
    say(&mut client, 3, &job, "completed", "").await;
    assert_eq!(line(&r.u, &job).await, "completed|-|-|-|true|true|true");
    // The progress is not the state's to change, and nothing here writes the audit log.
    assert_eq!(
        r.u.w
            .sql(&format!(
                "SELECT progress_permille FROM jobs WHERE id = '{job}'"
            ))
            .await,
        "0"
    );
    assert_eq!(r.u.w.sql("SELECT count(*) FROM audit_log").await, "0");
    assert_eq!(r.counts.applied.load(Ordering::Relaxed), 3);
    r.finish().await;
}

// J6
#[tokio::test]
async fn a_failure_keeps_its_code_the_procedures_code_and_the_detail_and_a_recovery_only_a_detail()
{
    let r = Rig::new().await;
    let failed = own_job(&r.u, &r.u.id, true).await;
    let refused = own_job(&r.u, &r.u.id, true).await;
    let recovery = own_job(&r.u, &r.u.id, true).await;
    let mut client = r.connect().await;
    say(
        &mut client,
        1,
        &failed,
        "failed",
        r#","failureCode":"procedure","procedureCode":"P-17","detail":"boom""#,
    )
    .await;
    assert_eq!(
        line(&r.u, &failed).await,
        "failed|procedure|P-17|boom|true|true|true"
    );
    say(
        &mut client,
        2,
        &refused,
        "preconditionFailed",
        r#","failureCode":"jobRefused","detail":"bad data""#,
    )
    .await;
    assert_eq!(
        line(&r.u, &refused).await,
        "preconditionFailed|jobRefused|-|bad data|true|true|true"
    );
    // A recovery has a detail and no failure code, even when the event gives one.
    say(
        &mut client,
        3,
        &recovery,
        "needsOnSiteRecovery",
        r#","failureCode":"internal","detail":"reconnect the cable""#,
    )
    .await;
    assert_eq!(
        line(&r.u, &recovery).await,
        "needsOnSiteRecovery|-|-|reconnect the cable|true|true|true"
    );
    assert_eq!(r.u.w.sql("SELECT count(*) FROM audit_log").await, "0");
    r.finish().await;
}

// J8
#[tokio::test]
async fn a_repeat_of_a_stored_event_is_not_applied_again() {
    let r = Rig::new().await;
    let job = own_job(&r.u, &r.u.id, true).await;
    let mut client = r.connect().await;
    say(&mut client, 1, &job, "running", "").await;
    let first = stamp(&r.u, &job, "last_reported_at").await;
    send(&mut client, &frame_of(1, &job, "running", "")).await;
    assert_eq!(read_ack(&mut client).await, 1, "answered with the cursor");
    until_count(&r.counts.duplicates, 1, "the repeats").await;
    assert_eq!(stamp(&r.u, &job, "last_reported_at").await, first);
    assert_eq!(r.counts.applied.load(Ordering::Relaxed), 1);
    r.finish().await;
}

// J9
#[tokio::test]
async fn a_received_after_a_later_state_only_stamps_the_report() {
    let r = Rig::new().await;
    let job = own_job(&r.u, &r.u.id, true).await;
    let mut client = r.connect().await;
    say(&mut client, 1, &job, "running", "").await;
    let at_running = stamp(&r.u, &job, "last_reported_at").await;
    say(&mut client, 2, &job, "received", "").await;
    assert_eq!(line(&r.u, &job).await, "running|-|-|-|false|true|true");
    assert!(
        is_later(
            &r.u,
            &stamp(&r.u, &job, "last_reported_at").await,
            &at_running
        )
        .await
    );
    say(&mut client, 3, &job, "completed", "").await;
    let ended = stamp(&r.u, &job, "finished_at").await;
    let reported = stamp(&r.u, &job, "last_reported_at").await;
    say(&mut client, 4, &job, "received", "").await;
    assert_eq!(line(&r.u, &job).await, "completed|-|-|-|true|true|true");
    assert_eq!(stamp(&r.u, &job, "finished_at").await, ended);
    assert_eq!(stamp(&r.u, &job, "last_reported_at").await, reported);
    r.finish().await;
}

// J10
#[tokio::test]
async fn the_same_state_twice_stamps_the_report_and_changes_nothing_else() {
    let r = Rig::new().await;
    let job = own_job(&r.u, &r.u.id, false).await;
    let mut client = r.connect().await;
    say(&mut client, 1, &job, "running", "").await;
    let (reported, delivered) = (
        stamp(&r.u, &job, "last_reported_at").await,
        stamp(&r.u, &job, "delivered_at").await,
    );
    say(&mut client, 2, &job, "running", "").await;
    assert_eq!(line(&r.u, &job).await, "running|-|-|-|false|true|true");
    assert!(
        is_later(
            &r.u,
            &stamp(&r.u, &job, "last_reported_at").await,
            &reported
        )
        .await
    );
    assert_eq!(stamp(&r.u, &job, "delivered_at").await, delivered);
    r.finish().await;
}

// J11
#[tokio::test]
async fn a_report_after_an_end_is_stored_and_acknowledged_and_logged_without_its_text() {
    let (log, _guard) = capture();
    let r = Rig::new().await;
    let job = own_job(&r.u, &r.u.id, true).await;
    let mut client = r.connect().await;
    say(&mut client, 1, &job, "completed", "").await;
    let before = line(&r.u, &job).await;
    // The first report after the end carries the detail; it takes the warning's gate.
    say(
        &mut client,
        2,
        &job,
        "failed",
        r#","failureCode":"internal","detail":"SECRET-DETAIL""#,
    )
    .await;
    say(&mut client, 3, &job, "running", "").await;
    assert_eq!(line(&r.u, &job).await, before);
    assert_eq!(rows(&r.u, &r.u.id).await.len(), 3);
    until_logged(&log, "were not applied").await;
    let text = log.text();
    let warning = text
        .lines()
        .find(|l| l.contains("were not applied"))
        .expect("the warning was written");
    assert!(warning.contains(&job) && warning.contains("completed") && warning.contains("failed"));
    assert!(!text.contains("SECRET-DETAIL"));
    r.finish().await;
}

// J12
#[tokio::test]
async fn a_job_in_needs_on_site_recovery_is_reopened_by_a_later_report() {
    let r = Rig::new().await;
    let job = own_job(&r.u, &r.u.id, true).await;
    let again = own_job(&r.u, &r.u.id, true).await;
    let mut client = r.connect().await;
    say(
        &mut client,
        1,
        &job,
        "needsOnSiteRecovery",
        r#","detail":"cable""#,
    )
    .await;
    assert_eq!(
        line(&r.u, &job).await,
        "needsOnSiteRecovery|-|-|cable|true|true|true"
    );
    // A `received` does not reopen it.
    say(&mut client, 2, &job, "received", "").await;
    assert_eq!(
        line(&r.u, &job).await,
        "needsOnSiteRecovery|-|-|cable|true|true|true"
    );
    // A state that is not an end clears the end and the detail; the job then ends once more.
    say(&mut client, 3, &job, "running", "").await;
    assert_eq!(line(&r.u, &job).await, "running|-|-|-|false|true|true");
    say(&mut client, 4, &job, "completed", "").await;
    assert_eq!(line(&r.u, &job).await, "completed|-|-|-|true|true|true");
    // An end replaces it too, with its own failure.
    say(
        &mut client,
        5,
        &again,
        "needsOnSiteRecovery",
        r#","detail":"cable""#,
    )
    .await;
    say(
        &mut client,
        6,
        &again,
        "failed",
        r#","failureCode":"vinMismatch","detail":"other car""#,
    )
    .await;
    assert_eq!(
        line(&r.u, &again).await,
        "failed|vinMismatch|-|other car|true|true|true"
    );
    r.finish().await;
}

// J13
#[tokio::test]
async fn a_report_changes_only_the_senders_own_job_of_its_own_tenant_named_in_canonical_form() {
    let r = Rig::new().await;
    let (other_id, other_token) = second_agent(&r.u).await;
    sqlx::query(
        "WITH t AS (INSERT INTO tenants (id, name) VALUES (gen_random_uuid(), 'other') RETURNING id) \
         UPDATE agents SET tenant_id = (SELECT id FROM t) WHERE id = $1::uuid",
    )
    .bind(&other_id)
    .execute(&r.u.w.pool)
    .await
    .unwrap();
    let (third_id, _) = second_agent(&r.u).await;
    let theirs = own_job(&r.u, &third_id, true).await;
    let own = own_job(&r.u, &r.u.id, true).await;
    let mut client = r.connect().await;
    let mut other = open(r.u.s.addr, &other_token).await;
    // Another agent's job of the same tenant, the job of another tenant named by an agent of the
    // other tenant's, a job nobody has, and an upper-case spelling of the sender's own.
    say(&mut client, 1, &theirs, "completed", "").await;
    say(&mut other, 1, &own, "completed", "").await;
    say(&mut client, 2, JOB, "completed", "").await;
    say(&mut client, 3, &own.to_uppercase(), "completed", "").await;
    // Something positive after them all.
    say(&mut client, 4, &own, "running", "").await;
    assert_eq!(line(&r.u, &theirs).await, "received|-|-|-|false|false|true");
    assert_eq!(line(&r.u, &own).await, "running|-|-|-|false|true|true");
    assert_eq!(r.counts.applied.load(Ordering::Relaxed), 1);
    r.finish().await;
}

// J14
#[tokio::test]
async fn a_job_the_server_ended_is_not_changed_by_a_report() {
    let r = Rig::new().await;
    let job = own_job(&r.u, &r.u.id, false).await;
    sqlx::query(
        "UPDATE jobs SET state = 'failed', failure_code = 'instructionRejected', \
             failure_detail = 'badSignature', finished_at = now(), delivered_at = NULL \
         WHERE id = $1::uuid",
    )
    .bind(&job)
    .execute(&r.u.w.pool)
    .await
    .unwrap();
    let before = line(&r.u, &job).await;
    assert_eq!(
        before,
        "failed|instructionRejected|-|badSignature|true|false|false"
    );
    let mut client = r.connect().await;
    say(&mut client, 1, &job, "running", "").await;
    say(&mut client, 2, &job, "completed", "").await;
    say(&mut client, 3, &job, "received", "").await;
    assert_eq!(line(&r.u, &job).await, before);
    r.finish().await;
}

// J15
#[tokio::test]
async fn a_received_report_settles_the_delivery_without_a_command_ack_and_a_late_ack_writes_nothing()
 {
    let r = Rig::with_lease(Duration::from_millis(300)).await;
    let job = add_job(&r.u.w, &r.u.id, 1, b"envelope").await;
    let mut client = r.connect().await;
    let (command_id, _) = command(&mut client).await;
    until_row(&r.u.w, &job, "received|1|true|false|||false").await;
    say(&mut client, 1, &job, "received", "").await;
    until_row(&r.u.w, &job, "received|1|true|true|||false").await;
    let delivered = stamp(&r.u, &job, "delivered_at").await;
    // Past the lease the job is not sent again.
    quiet_for(&mut client, 1000).await;
    ack(&mut client, &command_id).await;
    say(&mut client, 2, &job, "running", "").await;
    assert_eq!(row(&r.u.w, &job).await, "running|1|true|true|||false");
    assert_eq!(stamp(&r.u, &job, "delivered_at").await, delivered);
    r.finish().await;
}

// J16 (the acknowledgement)
#[tokio::test]
async fn a_state_report_settles_the_delivery_without_a_command_ack_and_a_late_ack_changes_nothing()
{
    let r = Rig::new().await;
    let job = add_job(&r.u.w, &r.u.id, 1, b"envelope").await;
    let mut client = r.connect().await;
    let (command_id, _) = command(&mut client).await;
    until_row(&r.u.w, &job, "received|1|true|false|||false").await;
    // No acknowledgement of the command came: the report's move sets the delivery.
    say(&mut client, 1, &job, "running", "").await;
    assert_eq!(row(&r.u.w, &job).await, "running|1|true|true|||false");
    let delivered = stamp(&r.u, &job, "delivered_at").await;
    ack(&mut client, &command_id).await;
    say(&mut client, 2, &job, "completed", "").await;
    assert_eq!(row(&r.u.w, &job).await, "completed|1|true|true|||true");
    assert_eq!(stamp(&r.u, &job, "delivered_at").await, delivered);
    r.finish().await;
}

// J16 (the rejection, in both orders)
#[tokio::test]
async fn a_report_and_a_rejection_of_the_command_leave_the_job_the_one_that_came_first() {
    let r = Rig::new().await;
    // The report first: the delivery is accepted, and the late rejection ends nothing.
    let first = add_job(&r.u.w, &r.u.id, 1, b"first").await;
    let mut client = r.connect().await;
    let (first_command, _) = command(&mut client).await;
    until_row(&r.u.w, &first, "received|1|true|false|||false").await;
    say(&mut client, 1, &first, "running", "").await;
    reject(&mut client, &first_command, "badSignature").await;
    // The rejection first: the job is ended by the server, and the report is not applied.
    let second = add_job(&r.u.w, &r.u.id, 2, b"second").await;
    let (second_command, _) = command(&mut client).await;
    until_row(&r.u.w, &second, "received|1|true|false|||false").await;
    reject(&mut client, &second_command, "badSignature").await;
    until_row(
        &r.u.w,
        &second,
        "failed|1|true|false|instructionRejected|badSignature|true",
    )
    .await;
    say(&mut client, 2, &second, "running", "").await;
    assert_eq!(
        row(&r.u.w, &second).await,
        "failed|1|true|false|instructionRejected|badSignature|true"
    );
    assert_eq!(row(&r.u.w, &first).await, "running|1|true|true|||false");
    r.finish().await;
}

// J20
#[tokio::test]
async fn a_refused_commit_leaves_the_job_as_it_was_and_the_retry_applies_the_report_once() {
    let (log, _guard) = capture();
    let r = Rig::new().await;
    let job = own_job(&r.u, &r.u.id, true).await;
    refuse_events(&r.u).await;
    let mut client = r.connect().await;
    send(&mut client, &frame_of(1, &job, "completed", "")).await;
    until_logged(&log, "could not store events").await;
    assert_eq!(line(&r.u, &job).await, "received|-|-|-|false|false|true");
    r.u.w
        .pg
        .execute("DROP TRIGGER test_events_refused ON agent_events")
        .await;
    assert_eq!(read_ack(&mut client).await, 1);
    assert_eq!(line(&r.u, &job).await, "completed|-|-|-|true|true|true");
    assert_eq!(r.counts.applied.load(Ordering::Relaxed), 1);
    r.finish().await;
}

// An end whose job was never marked delivered (its commandAck was lost) sets both times, and
// the queue's take and its retired-key sweep leave the job alone.
#[tokio::test]
async fn an_end_for_a_job_not_marked_delivered_sets_both_times_and_the_queue_leaves_it() {
    let r = Rig::with_lease(Duration::from_millis(300)).await;
    let job = add_job(&r.u.w, &r.u.id, 1, b"envelope").await;
    let mut client = r.connect().await;
    let _ = command(&mut client).await;
    until_row(&r.u.w, &job, "received|1|true|false|||false").await;
    say(&mut client, 1, &job, "completed", "").await;
    assert_eq!(row(&r.u.w, &job).await, "completed|1|true|true|||true");
    // Past the lease the job is not taken again.
    quiet_for(&mut client, 1000).await;
    // The sweep: the job's key is now retired, as is a second job's, which the sweep ends.
    sqlx::query("UPDATE jobs SET signing_key_id = $2 WHERE id = $1::uuid")
        .bind(&job)
        .bind("0".repeat(64))
        .execute(&r.u.w.pool)
        .await
        .unwrap();
    let stale = add_job_with(&r.u.w, &r.u.id, 2, b"stale", &"0".repeat(64), 3600).await;
    until_row(
        &r.u.w,
        &stale,
        "failed|0|false|false|instructionKeyRetired|the instruction was signed by a key that is no longer active|true",
    )
    .await;
    assert_eq!(row(&r.u.w, &job).await, "completed|1|true|true|||true");
    r.finish().await;
}

const LOCK_VIN: &str = "1HGCM82633A004352";

/// Gives the job the VIN and makes it the holder of the VIN's lock for an hour.
async fn hold_lock(u: &Up, job: &str) {
    sqlx::query("UPDATE jobs SET vin = $2 WHERE id = $1::uuid")
        .bind(job)
        .bind(LOCK_VIN)
        .execute(&u.w.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO vehicle_locks (tenant_id, vin, job_id, agent_id, expires_at) \
         SELECT tenant_id, vin, id, agent_id, now() + interval '1 hour' FROM jobs \
         WHERE id = $1::uuid",
    )
    .bind(job)
    .execute(&u.w.pool)
    .await
    .unwrap();
}

async fn holders(u: &Up) -> String {
    u.w.sql("SELECT coalesce(string_agg(job_id::text, ',' ORDER BY job_id), '') FROM vehicle_locks")
        .await
}

// A reopened needsOnSiteRecovery job runs with no lock; its later end deletes only its own.
#[tokio::test]
async fn a_reopened_recovery_jobs_later_end_leaves_the_lock_another_job_took() {
    let r = Rig::new().await;
    let first = own_job(&r.u, &r.u.id, true).await;
    let second = own_job(&r.u, &r.u.id, true).await;
    hold_lock(&r.u, &first).await;
    let mut client = r.connect().await;
    say(&mut client, 1, &first, "needsOnSiteRecovery", "").await;
    assert_eq!(holders(&r.u).await, "");
    // The first end freed the VIN, and the second job took it.
    hold_lock(&r.u, &second).await;
    say(&mut client, 2, &first, "running", "").await;
    say(&mut client, 3, &first, "completed", "").await;
    assert_eq!(line(&r.u, &first).await, "completed|-|-|-|true|true|true");
    assert_eq!(holders(&r.u).await, second);
    r.finish().await;
}

// E1 to E4 (ADR-487): a job the server expired is final. The agent's reports on it are stored and
// acknowledged and not applied; only a late `received` is quiet; the server's own end wrote no
// event row and moved no cursor.
#[tokio::test]
async fn reports_on_a_job_the_server_expired_are_stored_acknowledged_and_not_applied() {
    let (log, _guard) = capture();
    let r = Rig::new().await;
    let job = own_job(&r.u, &r.u.id, false).await;
    hold_lock(&r.u, &job).await;
    sqlx::query("UPDATE jobs SET start_deadline = now() - interval '1 second'")
        .execute(&r.u.w.pool)
        .await
        .unwrap();
    assert_eq!(server::job_expiry::expire(&r.u.w.pool).await.unwrap(), 1);
    let ended = line(&r.u, &job).await;
    assert_eq!(ended, "expired|-|-|-|true|false|false");
    // E4: no event row, and the agent's cursor has no row yet.
    assert_eq!(rows(&r.u, &r.u.id).await.len(), 0);
    assert_eq!(cursor(&r.u, &r.u.id).await, -1);
    let mut client = r.connect().await;
    // E1: a late `received`, then E2: the agent's own `expired`, then E3: a later state.
    say(&mut client, 1, &job, "received", "").await;
    say(&mut client, 2, &job, "expired", "").await;
    say(&mut client, 3, &job, "running", "").await;
    until_logged(&log, "were not applied").await;
    assert_eq!(line(&r.u, &job).await, ended);
    assert_eq!(holders(&r.u).await, "", "the lock is not taken again");
    assert_eq!(rows(&r.u, &r.u.id).await.len(), 3);
    assert_eq!(cursor(&r.u, &r.u.id).await, 3);
    assert_eq!(r.counts.applied.load(Ordering::Relaxed), 0);
    // The late `received` is quiet: the one warning is about what came after it.
    let text = log.text();
    let warning = text
        .lines()
        .find(|l| l.contains("were not applied"))
        .expect("the warning was written");
    assert!(
        warning.contains(&job) && warning.contains("expired"),
        "{warning}"
    );
    assert!(!warning.contains("received"), "{warning}");
    r.finish().await;
}

// A job whose agent_id is the sender's but whose tenant is another is not the sender's own.
#[tokio::test]
async fn a_job_of_the_sender_in_another_tenant_is_not_changed() {
    let r = Rig::new().await;
    let foreign = own_job(&r.u, &r.u.id, true).await;
    sqlx::query(
        "WITH t AS (INSERT INTO tenants (id, name) VALUES (gen_random_uuid(), 'other') \
             RETURNING id) \
         UPDATE jobs SET tenant_id = (SELECT id FROM t) WHERE id = $1::uuid",
    )
    .bind(&foreign)
    .execute(&r.u.w.pool)
    .await
    .unwrap();
    let own = own_job(&r.u, &r.u.id, true).await;
    let before = line(&r.u, &foreign).await;
    let mut client = r.connect().await;
    say(&mut client, 1, &foreign, "completed", "").await;
    // Something positive after it.
    say(&mut client, 2, &own, "running", "").await;
    assert_eq!(line(&r.u, &foreign).await, before);
    assert_eq!(line(&r.u, &own).await, "running|-|-|-|false|true|true");
    assert_eq!(r.counts.applied.load(Ordering::Relaxed), 1);
    r.finish().await;
}

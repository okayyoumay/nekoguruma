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
use crate::job_queue::{FRESH, add_job, row, session_token, settings, until_logged};
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
        let counts = Arc::new(EventCounts::default());
        let queue = Settings {
            events: Some(counts.clone()),
            ..settings(Duration::from_secs(600))
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
fn frame_of(seq: u64, job: &str, state: &str, extra: &str) -> String {
    format!(
        r#"{{"type":"agentEvent","seq":{seq},"kind":"jobState","jobId":"{job}","state":"{state}"{extra}}}"#
    )
}

fn frame(seq: u64) -> String {
    frame_of(seq, JOB, "running", "")
}

async fn send(client: &mut Client, body: &str) {
    client
        .send(Message::Text(body.to_owned().into()))
        .await
        .unwrap();
}

/// The next frame, which must be an `eventAck`; its number.
async fn read_ack(client: &mut Client) -> u64 {
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
    until_count(&first.seen, 2, "the first server's events seen").await;
    until_count(&second.seen, 2, "the second server's events seen").await;
    held.commit().await.unwrap();
    acked_up_to(&mut c1, 2).await;
    acked_up_to(&mut c2, 2).await;
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

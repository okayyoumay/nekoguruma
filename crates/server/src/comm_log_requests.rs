//! The operator's communication-log requests as rows (design 8.11, ADR-508): the statements that
//! write a request ([`request`], called by [`crate::comm_log_uploads`]), and the task of the hub's
//! set that delivers the requests to the agents and applies the agents' answers.
//!
//! Every [`Settings::period`] a pass first ends the requests that are [`EXPIRY`] old and still
//! `requested` (state `expired`, which stops the delivery and nothing else: the begin gate still
//! admits an agent's upload for it). Then it reads at most [`BATCH`] `requested` rows of the
//! agents the hub holds a connection for, that were not sent within [`Settings::lease`] and whose
//! job has no live `commLog` object, and sends `logUploadCommand` for each through
//! [`Hub::send_log_upload_command`]. Only a send the connection's queue took stamps the row's
//! `sent_at`, and the stamp is conditional on the state still being `requested`. A command is
//! therefore sent at least once to a connected agent, and again after each lease until the agent
//! answers or the request expires. It is not signed (ADR-508 item 4).
//!
//! Fairness and bounds: an agent has at most [`PER_AGENT`] rows in a pass, the lowest-ranked
//! first, and the pass sends in order with [`Hub::send_in_order`], which leaves an agent whose
//! queue is full or which is not connected alone for the rest of the pass, so one agent's backlog
//! never keeps another's row from being sent. Agents that are revoked are not sent to, and their
//! answers are not applied. The task also remembers, in memory, the connection each row was last
//! sent on, for as long as a lease: a row whose agent has since reconnected is due again at the
//! next pass without waiting for the lease, as a job is (ADR-476). After a restart of the server
//! the memory is empty and the lease alone applies. The memory holds at most the rows sent within
//! one lease.
//!
//! The answers reach the task through a bounded channel the delivery queue feeds with the
//! `logUploadAnswer` messages it reads from the hub's inbox ([`Answer`]). Each is applied by one
//! statement that changes a row only when it is the sending agent's, for that job and generation,
//! still `requested` and stamped `sent_at`: an answer from another agent, for an unknown row, for
//! a row already answered or expired, or for one that was written or renewed and not yet sent
//! changes nothing, so an answer to a command sent before a renewal cannot refuse the renewed
//! request (an answer that comes after the renewed row's next stamp is applied). `queued` makes the row `accepted`; any refusal makes it
//! `refused` with that reason. An answer that is lost, or fails to apply, is repeated by the
//! agent when the next command comes.
//!
//! The task ends when the stop begins, in its waits and in its statements, before the pool
//! closes. A failed pass or statement decides nothing, is logged at most once in [`LOG_EVERY`]
//! and is tried again at the next tick or answer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shared_proto::{AgentId, JobId, LogUploadOutcome, is_canonical_uuid};
use sqlx::PgPool;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::db::{failure, statement_timeouts};
use crate::health::may_log;
use crate::hub::{ConnectionId, Hub, OUTBOUND_CAPACITY};
use crate::session::now;
use crate::uploads::live_state;

/// The time between two passes. A request waits at most this long for its first send to an agent
/// that is connected, which an operator watches for; the pass is two short statements, so a
/// shorter period costs little, and a longer one makes a request that waits on a connect slow.
pub const PERIOD: Duration = Duration::from_secs(10);
/// How long a sent command waits for its answer before it is sent again (ADR-508 item 4).
pub const LEASE: Duration = Duration::from_secs(90);
/// How old a request that nobody answered may become before it is `expired` (ADR-508 item 4).
pub const EXPIRY: Duration = Duration::from_secs(24 * 60 * 60);
/// How many rows one pass sends: the oldest sent first, never-sent ones before any.
const BATCH: i64 = 100;
/// How many rows of one agent a pass reads: what an agent's outbound queue holds, since it takes
/// no more in one pass.
const PER_AGENT: i64 = OUTBOUND_CAPACITY as i64;
/// How many answers wait for the task. A full channel drops the answer, which the agent repeats.
pub(crate) const ANSWER_CAPACITY: usize = 64;
/// A failed pass or statement is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);

/// When the task passes. Tests shorten the period and the lease and count the passes.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct Settings {
    /// The time between two passes.
    pub period: Duration,
    /// How long a sent command waits for its answer before it is sent again.
    pub lease: Duration,
    /// Counts the passes that finished (a test waits on it).
    pub passes: Option<Arc<AtomicU64>>,
    /// Counts the passes that failed (a test waits on it).
    pub failures: Option<Arc<AtomicU64>>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            period: PERIOD,
            lease: LEASE,
            passes: None,
            failures: None,
        }
    }
}

/// An agent's answer to a `logUploadCommand`, with the agent that sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    pub(crate) agent: AgentId,
    pub(crate) job_id: JobId,
    pub(crate) generation: u64,
    pub(crate) outcome: LogUploadOutcome,
}

/// The state and refusal an answer writes.
fn written(outcome: LogUploadOutcome) -> (&'static str, Option<&'static str>) {
    match outcome {
        LogUploadOutcome::Queued => ("accepted", None),
        LogUploadOutcome::UnknownJob => ("refused", Some("unknownJob")),
        LogUploadOutcome::NoLog => ("refused", Some("noLog")),
        LogUploadOutcome::Busy => ("refused", Some("busy")),
        LogUploadOutcome::TooLarge => ("refused", Some("tooLarge")),
    }
}

/// Ends the requests that are older than `$1` seconds and still `requested`.
const EXPIRE_SQL: &str = "UPDATE comm_log_requests SET state = 'expired' \
     WHERE state = 'requested' AND requested_at < now() - make_interval(secs => $1)";

/// The rows to send: `requested`, of a connected agent (`$1`) that is not revoked, never sent, or
/// sent more than `$2` seconds ago, or in the keys `$4`, `$5`, `$6` (tenants, jobs, generations:
/// rows last sent on a connection that is gone), whose job and generation have no live `commLog`
/// object (the begin gate's definition of live). At most `$3` rows of each agent, the
/// longest-waiting first, and `$7` in all, the lowest ranks first, so that every agent with a row
/// gets its first before any agent gets its second.
const DUE_SQL: &str = concat!(
    "SELECT tenant_id::text, job_id::text, generation, agent_id::text FROM ( \
       SELECT r.tenant_id, r.job_id, r.generation, r.agent_id, r.sent_at, r.requested_at, \
         row_number() OVER (PARTITION BY r.agent_id \
           ORDER BY r.sent_at NULLS FIRST, r.requested_at) AS rank \
       FROM comm_log_requests r \
       JOIN agents a ON a.id = r.agent_id AND a.tenant_id = r.tenant_id AND a.revoked_at IS NULL \
       WHERE r.state = 'requested' AND r.agent_id = ANY($1::uuid[]) \
         AND (r.sent_at IS NULL OR r.sent_at < now() - make_interval(secs => $2) \
           OR (r.tenant_id, r.job_id, r.generation) IN \
             (SELECT * FROM unnest($4::uuid[], $5::uuid[], $6::bigint[]))) \
         AND NOT EXISTS (SELECT 1 FROM job_objects o WHERE o.tenant_id = r.tenant_id \
           AND o.job_id = r.job_id AND o.generation = r.generation AND o.kind = 'commLog' \
           AND ",
    live_state!("o"),
    ")) due WHERE rank <= $3 \
     ORDER BY rank, sent_at NULLS FIRST, requested_at LIMIT $7"
);

/// Stamps the rows the connections took (`$1` tenants, `$2` jobs, `$3` generations), unless an
/// answer or an expiry came first.
const STAMP_SQL: &str = "UPDATE comm_log_requests r SET sent_at = now() \
     FROM unnest($1::uuid[], $2::uuid[], $3::bigint[]) AS s(tenant_id, job_id, generation) \
     WHERE r.tenant_id = s.tenant_id AND r.job_id = s.job_id AND r.generation = s.generation \
       AND r.state = 'requested'";

/// Applies one answer: only the sending agent's row, for the job and generation, that is still
/// `requested` and has been sent (`$1` job, `$2` generation, `$3` the agent, `$4` the state, `$5`
/// the refusal). A row not yet sent has no command the answer could be to: it was written or
/// renewed after the command the answer is for.
const ANSWER_SQL: &str = "UPDATE comm_log_requests SET state = $4, refusal = $5, answered_at = now() \
     WHERE tenant_id = (SELECT tenant_id FROM agents WHERE id = $3::uuid AND revoked_at IS NULL) \
       AND job_id = $1::uuid AND generation = $2 AND agent_id = $3::uuid AND state = 'requested' \
       AND sent_at IS NOT NULL";

/// Writes a new request for the job (`$1` tenant, `$2` job, `$3` generation, `$4` the operator).
/// The agent is the job's own; the state, `requested_at` and the empty columns are the table's
/// defaults. A row that exists is left as it is, and so is a job whose log is stored (a
/// `finished` `commLog` object at the generation).
const INSERT_SQL: &str = "INSERT INTO comm_log_requests \
         (tenant_id, job_id, generation, agent_id, requested_by) \
     SELECT tenant_id, id, $3, agent_id, $4::uuid FROM jobs \
     WHERE tenant_id = $1::uuid AND id = $2::uuid \
       AND NOT EXISTS (SELECT 1 FROM job_objects o WHERE o.tenant_id = $1::uuid \
           AND o.job_id = $2::uuid AND o.generation = $3 AND o.kind = 'commLog' \
           AND o.state = 'finished') \
     ON CONFLICT (tenant_id, job_id, generation) DO NOTHING";

/// Renews a `refused` or `expired` request in place, in the one statement that keeps both of the
/// table's state checks true (same parameters as [`INSERT_SQL`]): back to `requested`, with the
/// refusal, the stamp and the answer's time cleared and the new request's operator and time.
/// A job whose log is stored is left as it is.
const RENEW_SQL: &str = "UPDATE comm_log_requests SET state = 'requested', refusal = NULL, \
         sent_at = NULL, answered_at = NULL, requested_by = $4::uuid, requested_at = now() \
     WHERE tenant_id = $1::uuid AND job_id = $2::uuid AND generation = $3 \
       AND state IN ('refused', 'expired') \
       AND NOT EXISTS (SELECT 1 FROM job_objects o WHERE o.tenant_id = $1::uuid \
           AND o.job_id = $2::uuid AND o.generation = $3 AND o.kind = 'commLog' \
           AND o.state = 'finished')";

/// The state and the refusal of a request (`$1` tenant, `$2` job, `$3` generation).
const STATE_SQL: &str = "SELECT state, refusal FROM comm_log_requests \
     WHERE tenant_id = $1::uuid AND job_id = $2::uuid AND generation = $3";

/// A request's row: tenant, job and generation.
type RowKey = (String, String, i64);

/// One row of [`DUE_SQL`].
struct Due {
    key: RowKey,
    job: JobId,
    agent: AgentId,
}

/// Where a row was last sent, and when: the agent, the connection and the time.
type Sent = HashMap<RowKey, (AgentId, ConnectionId, std::time::Instant)>;

/// Resolves when the stop has begun (or the hub is gone).
async fn stopped(stopping: &mut watch::Receiver<bool>) {
    let _ = stopping.wait_for(|stop| *stop).await;
}

/// Runs the passes and applies the answers until the stop. The task also ends when the
/// delivery queue, which feeds `answers`, has gone.
pub(crate) async fn run(
    hub: Hub,
    pool: PgPool,
    mut answers: mpsc::Receiver<Answer>,
    mut stopping: watch::Receiver<bool>,
    settings: Settings,
) {
    // A tick missed because the task was slow gives one pass, not a burst.
    let mut ticks = interval_at(Instant::now() + settings.period, settings.period);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let pass_gate = Mutex::new(None);
    let answer_gate = Mutex::new(None);
    let mut sent = Sent::new();
    loop {
        tokio::select! {
            () = stopped(&mut stopping) => return,
            _ = ticks.tick() => {
                tokio::select! {
                    () = stopped(&mut stopping) => return,
                    done = pass(&hub, &pool, settings.lease, &mut sent) => match done {
                        Ok(()) => {
                            if let Some(passes) = &settings.passes {
                                passes.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(error) => {
                            log_failure(&pass_gate, &error, "the log request pass failed");
                            if let Some(failures) = &settings.failures {
                                failures.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    },
                }
            }
            answer = answers.recv() => {
                let Some(answer) = answer else { return };
                tokio::select! {
                    () = stopped(&mut stopping) => return,
                    done = apply(&pool, &answer) => {
                        if let Err(error) = done {
                            log_failure(&answer_gate, &error, "a log upload answer could not be applied");
                        }
                    }
                }
            }
        }
    }
}

fn log_failure(gate: &Mutex<Option<std::time::Instant>>, error: &sqlx::Error, what: &str) {
    if may_log(gate, now(), LOG_EVERY) {
        let why = failure(error);
        tracing::warn!(?why, "{what}; it is tried again");
    }
}

/// One pass: expires the old requests, then sends the due ones and stamps what was sent.
async fn pass(
    hub: &Hub,
    pool: &PgPool,
    lease: Duration,
    sent: &mut Sent,
) -> Result<(), sqlx::Error> {
    let connections: HashMap<AgentId, ConnectionId> = hub.connected().into_iter().collect();
    let at = now();
    // After a lease the stamp alone decides.
    sent.retain(|_, (_, _, when)| at.duration_since(*when) < lease);
    // The rows whose agent has a newer connection than the one they were sent on are due again.
    let stale: Vec<RowKey> = sent
        .iter()
        .filter(|(_, (agent, connection, _))| {
            connections.get(agent).is_some_and(|now| now != connection)
        })
        .map(|(key, _)| key.clone())
        .collect();
    let agents: Vec<String> = connections.keys().map(|agent| agent.0.clone()).collect();
    let due = read(pool, &agents, lease, &stale).await?;
    // A stale row that is not due any more was answered, expired or stored: forget it.
    for key in &stale {
        if !due.iter().any(|d| d.key == *key) {
            sent.remove(key);
        }
    }
    let mut stamped: (Vec<String>, Vec<String>, Vec<i64>) = Default::default();
    for (due, connection) in send_due(hub, due) {
        let (tenant, job, generation) = due.key.clone();
        stamped.0.push(tenant);
        stamped.1.push(job);
        stamped.2.push(generation);
        sent.insert(due.key, (due.agent, connection, now()));
    }
    if stamped.0.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    statement_timeouts(&mut tx).await?;
    sqlx::query(STAMP_SQL)
        .bind(stamped.0)
        .bind(stamped.1)
        .bind(stamped.2)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// Sends the command of each due row in order, and returns the rows whose connection's queue took
/// it, with that connection. A row whose agent's queue is full or which has left since the
/// snapshot is not returned, so it stays unstamped and is due at the next pass; the agent's later
/// rows are not tried this pass.
fn send_due(hub: &Hub, due: Vec<Due>) -> Vec<(Due, ConnectionId)> {
    Hub::send_in_order(
        due,
        |d| &d.agent,
        || false,
        |d| {
            let generation = u64::try_from(d.key.2).unwrap_or_default();
            hub.send_log_upload_command(&d.agent, &d.job, generation)
        },
    )
    .into_iter()
    // The only other result is a frame that cannot be encoded, which a UUID never makes.
    .filter_map(|(due, result)| result.ok().map(|connection| (due, connection)))
    .collect()
}

/// Expires the old requests and reads the due ones, in one transaction. `stale` are the rows
/// that are due whatever their stamp says.
async fn read(
    pool: &PgPool,
    connected: &[String],
    lease: Duration,
    stale: &[RowKey],
) -> Result<Vec<Due>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    statement_timeouts(&mut tx).await?;
    sqlx::query(EXPIRE_SQL)
        .bind(EXPIRY.as_secs_f64())
        .execute(&mut *tx)
        .await?;
    let rows: Vec<(String, String, i64, String)> = if connected.is_empty() {
        Vec::new()
    } else {
        let tenants: Vec<&str> = stale.iter().map(|k| k.0.as_str()).collect();
        let jobs: Vec<&str> = stale.iter().map(|k| k.1.as_str()).collect();
        let generations: Vec<i64> = stale.iter().map(|k| k.2).collect();
        sqlx::query_as(DUE_SQL)
            .bind(connected)
            .bind(lease.as_secs_f64())
            .bind(PER_AGENT)
            .bind(tenants)
            .bind(jobs)
            .bind(generations)
            .bind(BATCH)
            .fetch_all(&mut *tx)
            .await?
    };
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|(tenant, job, generation, agent)| Due {
            key: (tenant, job.clone(), generation),
            job: JobId(job),
            agent: AgentId(agent),
        })
        .collect())
}

/// What [`request`] did to the job's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Written {
    /// There was no row; one was inserted, `requested`.
    Inserted,
    /// The row was `refused` or `expired` and is `requested` again.
    Renewed,
    /// The row was `requested` or `accepted` (or another transaction wrote it first), and is as
    /// it was.
    Left,
}

/// A request's state, and its refusal when it is `refused`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestState {
    pub(crate) state: String,
    pub(crate) refusal: Option<String>,
}

/// Writes the operator's request for the job's log at `generation`, in the caller's transaction
/// (ADR-508 item 1): inserts a row, or renews one that is `refused` or `expired`, or leaves one
/// that is `requested` or `accepted`, or whose job's log is stored. The caller has judged that the operator may ask. Two
/// requests for one row meet at its key: the second waits for the first's commit and then finds
/// the row it wrote.
pub(crate) async fn request(
    tx: &mut sqlx::PgConnection,
    tenant: &str,
    job: &str,
    generation: i64,
    operator: &str,
) -> Result<Written, sqlx::Error> {
    let inserted = sqlx::query(INSERT_SQL)
        .bind(tenant)
        .bind(job)
        .bind(generation)
        .bind(operator)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if inserted > 0 {
        return Ok(Written::Inserted);
    }
    let renewed = sqlx::query(RENEW_SQL)
        .bind(tenant)
        .bind(job)
        .bind(generation)
        .bind(operator)
        .execute(tx)
        .await?
        .rows_affected();
    Ok(if renewed > 0 {
        Written::Renewed
    } else {
        Written::Left
    })
}

/// The job's request at `generation` as it is now, if there is one.
pub(crate) async fn state(
    tx: &mut sqlx::PgConnection,
    tenant: &str,
    job: &str,
    generation: i64,
) -> Result<Option<RequestState>, sqlx::Error> {
    let row: Option<(String, Option<String>)> = sqlx::query_as(STATE_SQL)
        .bind(tenant)
        .bind(job)
        .bind(generation)
        .fetch_optional(tx)
        .await?;
    Ok(row.map(|(state, refusal)| RequestState { state, refusal }))
}

/// Applies `answer` to the sender's own `requested`, sent row, if there is one. A job ID that is not a
/// canonical UUID, or a generation beyond `bigint`, names no row.
async fn apply(pool: &PgPool, answer: &Answer) -> Result<(), sqlx::Error> {
    let Ok(generation) = i64::try_from(answer.generation) else {
        return Ok(());
    };
    if !is_canonical_uuid(&answer.job_id.0) {
        return Ok(());
    }
    let (state, refusal) = written(answer.outcome);
    let mut tx = pool.begin().await?;
    statement_timeouts(&mut tx).await?;
    sqlx::query(ANSWER_SQL)
        .bind(&answer.job_id.0)
        .bind(generation)
        .bind(&answer.agent.0)
        .bind(state)
        .bind(refusal)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn due(agent: &str, n: i64) -> Due {
        let job = format!("0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d{n:02}");
        Due {
            key: ("t".to_owned(), job.clone(), 1),
            job: JobId(job),
            agent: AgentId(agent.to_owned()),
        }
    }

    #[test]
    fn a_row_the_queue_refused_is_not_returned_and_another_agents_row_is_still_sent() {
        let hub = Hub::new();
        let (full, roomy) = (AgentId("full".to_owned()), AgentId("roomy".to_owned()));
        let mut full_registration = hub.register(&full).unwrap();
        let roomy_registration = hub.register(&roomy).unwrap();
        for _ in 0..OUTBOUND_CAPACITY {
            hub.send_job_command(&full, "j".to_owned()).unwrap();
        }
        let sent = send_due(
            &hub,
            vec![
                due("full", 1),
                due("full", 2),
                due("roomy", 3),
                due("gone", 4),
            ],
        );
        let keys: Vec<_> = sent.iter().map(|(d, c)| (d.agent.0.as_str(), *c)).collect();
        assert_eq!(keys, [("roomy", roomy_registration.id)]);
        // The queue of the full agent took nothing more.
        for _ in 0..OUTBOUND_CAPACITY {
            full_registration.outbound.try_recv().unwrap();
        }
        assert!(full_registration.outbound.try_recv().is_err());
    }
}

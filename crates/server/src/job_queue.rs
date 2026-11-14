//! The job delivery queue (design 5.3, 12, 14.1; ADR-476): one task of the hub's set that polls the
//! `jobs` table and puts each job's stored instruction on the connection of the agent it is for,
//! and the only consumer of [`Hub::inbox`].
//!
//! A pass is one transaction: it ends the jobs signed by a key that is no longer active, takes the
//! rows of the agents connected to this instance (the oldest few of each, at most one connection's
//! worth, ranked within the agent, and in all at most [`PASS_LIMIT`] rows, the lowest ranks first,
//! so that every agent with work gets its oldest job into the pass before any agent gets its
//! second, and there is no state between passes), locks them with `FOR UPDATE SKIP LOCKED`, queues
//! a frame for each through the hub (which never waits) and marks the rows it queued, so that two
//! instances never hold one row at once. A mark that is not acknowledged is released when its
//! connection is no longer current, and expires after the lease: delivery is at least once (a
//! commit that fails after the frame was queued leaves the row as it was before the send, or, when
//! the commit went through though it was reported as failed, at the new mark; either way it can
//! be sent again), and a resend carries the same stored bytes under a new command ID. The agent's
//! answers settle a delivery by connection and command ID, and write only while the row still
//! carries a mark token the delivery expects (the one the row had when it was taken, and the one
//! the mark returned, until the commit is known); a rejection ends the job, and nothing is resent
//! after it.
//!
//! The task ends when the stop begins, in its waits and in the pass's read; once a frame has been
//! queued the pass finishes its mark. While a pass or a write waits on the database the inbox is
//! still read into a buffer, so that the database never fills it, and an answer that could not be
//! written is kept for the next attempt. The steps that need no database ([`dispatch`],
//! [`Queue::step`] and [`draining`]) are plain functions, so they are tested without one.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use shared_proto::{AgentId, AgentMessage};
use sqlx::{PgConnection, PgPool};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::db::{failure, statement_timeouts};
use crate::health::may_log;
use crate::hub::{ConnectionId, Delivery, Hub, Inbound, OUTBOUND_CAPACITY, SendError};
use crate::session::now;

/// A failed pass or write is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);
/// How many rows one pass reads for one agent: what the agent's outbound queue holds.
const PER_AGENT_LIMIT: i64 = OUTBOUND_CAPACITY as i64;
/// How many rows one pass takes in all, so that its transaction stays short.
const PASS_LIMIT: i64 = 64;
// A single agent with a backlog can still use a whole queue's worth of the pass.
const _: () = assert!(PASS_LIMIT >= PER_AGENT_LIMIT);
/// How many times an iteration applies what was buffered during a write before it goes on.
const APPLY_ROUNDS: usize = 4;
/// How many messages are buffered while the database is waited for; past this the inbox fills and
/// the connections stop reading, as they do for any slow consumer.
const HELD_LIMIT: usize = 4096;

/// When the queue polls and how long an unanswered delivery is kept (ADR-476). Tests shorten both.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct Settings {
    /// The time between two passes.
    pub period: Duration,
    /// How long a delivery may wait for its answer before the job can be taken again.
    pub lease: Duration,
    /// Counts the passes that read the database and finished (a test waits on it to know that a
    /// pass ran).
    pub passes: Option<Arc<AtomicU64>>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            period: Duration::from_secs(2),
            lease: Duration::from_secs(90),
            passes: None,
        }
    }
}

/// The take: `$1` the connected agents (`text[]`), `$2` the lease in seconds, `$3` the jobs that
/// are in flight in this process, `$4` the limit for each agent, `$5` the limit for the pass.
///
/// `ranked` reads, for each agent, its oldest `$4` rows (a lateral select, which lets the planner
/// reach them through `jobs_pending`) and numbers them within the agent. `chosen` keeps the first
/// `$5` of all by (position, created, id): the oldest job of every agent before any agent's second,
/// so that an agent with many old jobs, or one that takes nothing, holds back no other agent's
/// first job, and an agent with no work takes no place. Neither step locks: a window function
/// cannot be combined with `FOR UPDATE`, and locking before the limit would hold rows the limit
/// then drops. The last step locks only the chosen rows, skipping those another pass holds (that
/// pass commits, so the rows are free again by the next pass; two instances cannot skip each
/// other's rows for good), and checks the conditions again on the row it locked, as the row may
/// have changed since it was read. The last column is the row's mark before the send (null, or an
/// expired one), the token a delivery whose mark does not commit still expects.
#[doc(hidden)]
pub const TAKE_SQL: &str = "\
WITH ranked AS ( \
  SELECT c.tenant_id, c.id, c.created_at, \
         row_number() OVER (PARTITION BY c.agent_id ORDER BY c.created_at, c.id) AS pos \
  FROM agents a CROSS JOIN LATERAL ( \
    SELECT t.tenant_id, t.id, t.agent_id, t.created_at \
    FROM jobs t \
    WHERE t.tenant_id = a.tenant_id AND t.agent_id = a.id \
      AND t.state = 'received' AND t.finished_at IS NULL AND t.delivered_at IS NULL \
      AND (t.delivery_sent_at IS NULL OR t.delivery_sent_at < now() - make_interval(secs => $2::float8)) \
      AND t.start_deadline > now() \
      AND t.signing_key_id = (SELECT key_id FROM instruction_keys WHERE retired_at IS NULL) \
      AND t.id <> ALL($3::uuid[]) \
    ORDER BY t.created_at, t.id \
    LIMIT $4 \
  ) c \
  WHERE a.revoked_at IS NULL AND a.id = ANY($1::uuid[]) \
), chosen AS ( \
  SELECT tenant_id, id, created_at, pos FROM ranked ORDER BY pos, created_at, id LIMIT $5 \
) \
SELECT j.tenant_id::text, j.id::text, j.agent_id::text, j.signed_job, j.delivery_attempts, \
       j.delivery_sent_at::text \
FROM chosen c JOIN jobs j ON j.tenant_id = c.tenant_id AND j.id = c.id \
WHERE j.state = 'received' AND j.finished_at IS NULL AND j.delivered_at IS NULL \
  AND (j.delivery_sent_at IS NULL OR j.delivery_sent_at < now() - make_interval(secs => $2::float8)) \
  AND j.start_deadline > now() \
ORDER BY c.pos, c.created_at, c.id \
FOR UPDATE OF j SKIP LOCKED";

/// Ends the undelivered jobs whose key is not the active one and whose instruction is not
/// outstanding, skipping rows another pass holds; with no active key the comparison is null and
/// nothing is ended.
const SWEEP_SQL: &str = "\
UPDATE jobs SET state = 'failed', failure_code = 'instructionKeyRetired', \
    failure_detail = 'the instruction was signed by a key that is no longer active', \
    finished_at = now() \
WHERE (tenant_id, id) IN ( \
  SELECT tenant_id, id FROM jobs \
  WHERE state = 'received' AND finished_at IS NULL AND delivered_at IS NULL \
    AND (delivery_sent_at IS NULL OR delivery_sent_at < now() - make_interval(secs => $1::float8)) \
    AND signing_key_id <> (SELECT key_id FROM instruction_keys WHERE retired_at IS NULL) \
  FOR UPDATE SKIP LOCKED)";

/// The mark is the time of the statement, not of the transaction, so that the lease counts from
/// the moment the frame was queued; it is returned, and kept in the flight as the token the
/// answer's writes check once the commit has succeeded.
const MARK_SQL: &str = "\
UPDATE jobs SET delivery_attempts = delivery_attempts + 1, delivery_sent_at = statement_timestamp() \
WHERE tenant_id = $1::uuid AND id = $2::uuid \
RETURNING delivery_sent_at::text";

const TOO_LARGE_SQL: &str = "\
UPDATE jobs SET state = 'failed', failure_code = 'internal', \
    failure_detail = 'the instruction is too large to send', finished_at = now() \
WHERE tenant_id = $1::uuid AND id = $2::uuid";

/// The condition on the row's mark and attempts that the answer's writes share, with the numbers of
/// the parameters it uses: the attempt number this delivery recorded (n + 1), the mark the row had
/// when it was taken (null for none), the mark the database returned (null when none came), and
/// whether the prior mark still counts.
///
/// A mark whose commit is not known leaves the row in one of two states: untouched, with the
/// prior mark and n attempts, or marked, with the returned mark and n + 1 attempts. So the prior
/// mark goes with n (one less than the delivery's number) and the returned mark with n + 1; each
/// pair alone would accept a row another delivery has since marked (the same mark under a later
/// attempt, or the same attempt under a later mark). Once the commit is known to have succeeded the
/// prior mark no longer counts, as a row whose mark was cleared must not match it.
macro_rules! mark_guard {
    ($attempts:literal, $prior:literal, $returned:literal, $prior_counts:literal) => {
        concat!(
            "AND ((",
            $prior_counts,
            "::bool AND delivery_sent_at IS NOT DISTINCT FROM ",
            $prior,
            "::timestamptz AND delivery_attempts = ",
            $attempts,
            "::int4 - 1) \
              OR (delivery_sent_at = ",
            $returned,
            "::timestamptz AND delivery_attempts = ",
            $attempts,
            "::int4))"
        )
    };
}

/// The attempts are raised to the answered delivery's, in case its mark's commit failed. `$3` is
/// the delivery's attempt number, `$4` to `$6` the tokens of [`mark_guard`]: a row that carries one
/// of them is settled, a row another delivery has marked since is not touched.
const DELIVERED_SQL: &str = concat!(
    "UPDATE jobs SET delivered_at = now(), delivery_attempts = GREATEST(delivery_attempts, $3) \
     WHERE tenant_id = $1::uuid AND id = $2::uuid AND finished_at IS NULL AND delivered_at IS NULL ",
    mark_guard!("$3", "$4", "$5", "$6")
);

/// `$3` is the reason, `$4` the delivery's attempt number, `$5` to `$7` the tokens.
const REJECTED_SQL: &str = concat!(
    "UPDATE jobs SET state = 'failed', failure_code = 'instructionRejected', failure_detail = $3, \
         finished_at = now() \
     WHERE tenant_id = $1::uuid AND id = $2::uuid AND state = 'received' \
       AND finished_at IS NULL AND delivered_at IS NULL ",
    mark_guard!("$4", "$5", "$6", "$7")
);

/// Clears the mark only while it is still this delivery's (attempt number and tokens, as above).
const RELEASE_SQL: &str = concat!(
    "UPDATE jobs SET delivery_sent_at = NULL \
     WHERE tenant_id = $1::uuid AND id = $2::uuid AND delivered_at IS NULL AND finished_at IS NULL ",
    mark_guard!("$3", "$4", "$5", "$6")
);

/// A row as the take returns it.
type TakenRow = (String, String, String, Vec<u8>, i32, Option<String>);

/// A row the take returned.
#[derive(Debug)]
struct Taken {
    tenant: String,
    id: String,
    agent: AgentId,
    signed_job: Vec<u8>,
    /// The attempts recorded before this one.
    attempts: i32,
    /// The row's mark (text) when it was taken: null, or an expired one.
    prior_mark: Option<String>,
}

/// What [`dispatch`] did with the taken rows.
#[derive(Debug, Default)]
struct Dispatched {
    sent: Vec<(Taken, Delivery)>,
    too_large: Vec<Taken>,
}

/// Queues the taken rows, in the order given. An agent whose queue is full or which has no
/// current connection gets nothing more this pass, so its later jobs keep their place; a frame
/// over the limit can never be sent and ends its job. `stopped` is asked before each send.
fn dispatch(hub: &Hub, taken: Vec<Taken>, stopped: impl Fn() -> bool) -> Dispatched {
    let mut out = Dispatched::default();
    let mut skipped: HashSet<AgentId> = HashSet::new();
    for job in taken {
        if stopped() {
            break;
        }
        if skipped.contains(&job.agent) {
            continue;
        }
        match hub.send_job_command(&job.agent, STANDARD.encode(&job.signed_job)) {
            Ok(delivery) => out.sent.push((job, delivery)),
            Err(SendError::TooLarge) => out.too_large.push(job),
            Err(SendError::Busy | SendError::NotConnected) => {
                skipped.insert(job.agent);
            }
        }
    }
    out
}

/// A delivery that has been queued and is not settled yet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Flight {
    tenant: String,
    id: String,
    agent: AgentId,
    /// The attempt number this delivery recorded.
    attempts: i32,
    sent_at: std::time::Instant,
    /// The mark (text) the row had when it was taken: null, or an expired one. The row carries it
    /// while the mark's commit has not gone through.
    prior: Option<String>,
    /// The mark the database returned, when it returned one: the row carries it once the commit
    /// has gone through, which a commit that was reported as failed may have done.
    returned: Option<String>,
    /// The mark's commit succeeded, so the row carries `returned` and `prior` no longer counts.
    committed: bool,
}

/// What an answer or a departure asks the database to write.
#[derive(Debug, Default, PartialEq, Eq)]
struct Step {
    delivered: Vec<Flight>,
    rejected: Vec<(Flight, String)>,
    released: Vec<Flight>,
}

impl Step {
    fn is_empty(&self) -> bool {
        self.delivered.is_empty() && self.rejected.is_empty() && self.released.is_empty()
    }

    /// Adds what `other` asks to this step, which was not written yet.
    fn append(&mut self, mut other: Step) {
        self.delivered.append(&mut other.delivered);
        self.rejected.append(&mut other.rejected);
        self.released.append(&mut other.released);
    }

    /// The jobs this step still has to write about; the take leaves them out.
    fn ids(&self) -> impl Iterator<Item = String> + '_ {
        let rejected = self.rejected.iter().map(|(flight, _)| flight);
        self.delivered
            .iter()
            .chain(rejected)
            .chain(&self.released)
            .map(|flight| flight.id.clone())
    }
}

/// The deliveries in flight, by connection and command ID. It is the memory of one process: the
/// database holds only the marks.
#[derive(Debug, Default)]
struct Queue {
    flights: HashMap<(ConnectionId, String), Flight>,
}

impl Queue {
    /// Remembers what a pass queued, before its mark is committed: a commit that fails leaves
    /// the row as it was, and the entry keeps it from being taken again here.
    fn register(&mut self, sent: &[(Taken, Delivery)], at: std::time::Instant) {
        for (job, delivery) in sent {
            self.flights.insert(
                (delivery.connection, delivery.command_id.clone()),
                Flight {
                    tenant: job.tenant.clone(),
                    id: job.id.clone(),
                    agent: job.agent.clone(),
                    attempts: job.attempts + 1,
                    sent_at: at,
                    prior: job.prior_mark.clone(),
                    returned: None,
                    committed: false,
                },
            );
        }
    }

    /// Records the marks the database returned (job, attempt number, mark). When the commit
    /// succeeded only the returned mark counts from then on; when it failed the row may have gone
    /// either way (a failure can be reported for a commit that happened), and both are kept.
    fn set_marks(&mut self, marks: &[(String, i32, String)], committed: bool) {
        for flight in self.flights.values_mut() {
            if let Some((_, _, mark)) = marks
                .iter()
                .find(|(id, attempts, _)| *id == flight.id && *attempts == flight.attempts)
            {
                flight.returned = Some(mark.clone());
                flight.committed = committed;
            }
        }
    }

    /// The jobs in flight, which the take leaves out.
    fn in_flight(&self) -> Vec<String> {
        self.flights.values().map(|f| f.id.clone()).collect()
    }

    /// Applies what the inbox gave and what `snapshot` (the connections taken before the inbox
    /// was drained) shows. An answer settles the flight with its connection and command ID, from
    /// the agent that owns it; nothing else does, so an unknown or repeated answer is ignored.
    /// Then the flights whose connection is no longer current are released, and those older than
    /// `lease` are forgotten (their mark expires by itself). An answer read before its connection
    /// left is applied before the release; one read after the snapshot matches nothing the next
    /// time, which can cost one duplicate (design 5.3).
    fn step(
        &mut self,
        snapshot: &[(AgentId, ConnectionId)],
        inbound: Vec<Inbound>,
        at: std::time::Instant,
        lease: Duration,
    ) -> Step {
        let mut step = Step::default();
        for item in inbound {
            let (command_id, reason) = match item.message {
                AgentMessage::CommandAck { command_id } => (command_id, None),
                AgentMessage::CommandRejected { command_id, reason } => (command_id, Some(reason)),
                other => {
                    tracing::debug!(
                        agent_id = %item.agent.0,
                        kind = ?std::mem::discriminant(&other),
                        "the delivery queue drops a message that is not an answer to a command"
                    );
                    continue;
                }
            };
            let key = (item.connection, command_id);
            if self.flights.get(&key).is_none_or(|f| f.agent != item.agent) {
                continue;
            }
            let flight = self.flights.remove(&key).expect("checked above");
            match reason {
                None => step.delivered.push(flight),
                Some(reason) => {
                    let name = serde_json::to_value(reason)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_else(|| "unknown".to_owned());
                    step.rejected.push((flight, name));
                }
            }
        }
        let current: HashSet<ConnectionId> = snapshot.iter().map(|(_, id)| *id).collect();
        self.flights.retain(|(connection, _), flight| {
            if !current.contains(connection) {
                step.released.push(flight.clone());
                false
            } else {
                at.duration_since(flight.sent_at) < lease
            }
        });
        step
    }
}

/// Resolves when the stop has begun (or the hub is gone).
async fn stopped(stopping: &mut watch::Receiver<bool>) {
    let _ = stopping.wait_for(|stop| *stop).await;
}

/// Runs `work`, which waits on the database, and meanwhile reads the inbox into `held` (up to
/// [`HELD_LIMIT`] messages), so that a slow database does not fill the inbox and stop the
/// connections reading. The messages are applied by the next iteration.
async fn draining<T>(
    inbox: &mut mpsc::Receiver<Inbound>,
    held: &mut Vec<Inbound>,
    work: impl Future<Output = T>,
) -> T {
    let mut work = std::pin::pin!(work);
    loop {
        tokio::select! {
            out = &mut work => return out,
            item = inbox.recv(), if held.len() < HELD_LIMIT => match item {
                Some(item) => held.push(item),
                // No sender is left: nothing more comes.
                None => return work.await,
            },
        }
    }
}

/// Runs the queue until the stop, with the receiver of the hub's inbox.
pub(crate) async fn run(
    hub: Hub,
    pool: PgPool,
    mut inbox: mpsc::Receiver<Inbound>,
    mut stopping: watch::Receiver<bool>,
    settings: Settings,
) {
    let mut ticks = interval_at(Instant::now() + settings.period, settings.period);
    // A tick missed because the task was slow gives one pass, not a burst.
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let gate = Mutex::new(None);
    let mut queue = Queue::default();
    // The messages read while the database was waited for, and what has not been written yet.
    let mut held: Vec<Inbound> = Vec::new();
    let mut carry = Step::default();
    // The last write failed: it is retried on the tick, not on every message.
    let mut failed = false;
    let mut marks: Vec<(String, i32, String)> = Vec::new();
    loop {
        let due = tokio::select! {
            biased;
            () = stopped(&mut stopping) => return,
            _ = ticks.tick() => true,
            first = inbox.recv() => match first {
                Some(first) => {
                    held.push(first);
                    false
                }
                None => return,
            },
        };
        // The connections first, then the inbox: an answer read before its connection left is
        // in the inbox (or already held) by then, so it is applied before the release. What is
        // buffered while a write waits is applied at once, without waiting for the next tick.
        let mut snapshot = Vec::new();
        for _ in 0..APPLY_ROUNDS {
            snapshot = hub.connected();
            while let Ok(item) = inbox.try_recv() {
                held.push(item);
            }
            carry.append(queue.step(&snapshot, std::mem::take(&mut held), now(), settings.lease));
            if carry.is_empty() || (failed && !due) {
                break;
            }
            // A write that fails keeps the step, and the next tick writes it again.
            match draining(&mut inbox, &mut held, write(&pool, &carry)).await {
                Ok(()) => {
                    carry = Step::default();
                    failed = false;
                }
                Err(error) => {
                    failed = true;
                    warn(&gate, &error, "the delivery queue could not record answers");
                    break;
                }
            }
            if held.is_empty() {
                break;
            }
        }
        if !due {
            continue;
        }
        let skip: Vec<String> = queue.in_flight().into_iter().chain(carry.ids()).collect();
        let read = tokio::select! {
            () = stopped(&mut stopping) => return,
            read = draining(&mut inbox, &mut held, read(&pool, &snapshot, skip, settings.lease)) => read,
        };
        match read {
            Ok((tx, taken)) => {
                // From here the pass is not cancelled: a frame that is queued gets its mark.
                let dispatched = dispatch(&hub, taken, || *stopping.borrow());
                queue.register(&dispatched.sent, now());
                marks.clear();
                let marked =
                    draining(&mut inbox, &mut held, mark(tx, &dispatched, &mut marks)).await;
                match marked {
                    Ok(()) => {
                        queue.set_marks(&marks, true);
                        if let Some(passes) = &settings.passes {
                            passes.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(error) => {
                        // The commit may have happened though it failed: keep both tokens.
                        queue.set_marks(&marks, false);
                        warn(
                            &gate,
                            &error,
                            "the delivery queue could not mark deliveries",
                        );
                    }
                }
            }
            Err(error) => warn(&gate, &error, "the delivery pass failed; nothing was sent"),
        }
    }
}

fn warn(gate: &Mutex<Option<std::time::Instant>>, error: &sqlx::Error, what: &str) {
    if may_log(gate, now(), LOG_EVERY) {
        let why = failure(error);
        tracing::warn!(?why, "{what}");
    }
}

/// The first half of a pass: ends the jobs of a retired key and takes the rows of the connected
/// agents. The transaction stays open, holding the locks.
async fn read(
    pool: &PgPool,
    connected: &[(AgentId, ConnectionId)],
    skip: Vec<String>,
    lease: Duration,
) -> Result<(sqlx::Transaction<'static, sqlx::Postgres>, Vec<Taken>), sqlx::Error> {
    let mut tx = pool.begin().await?;
    statement_timeouts(&mut tx).await?;
    let ended = sqlx::query(SWEEP_SQL)
        .bind(lease.as_secs_f64())
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if ended > 0 {
        tracing::info!(jobs = ended, "jobs signed by a retired key were ended");
    }
    if connected.is_empty() {
        return Ok((tx, Vec::new()));
    }
    let agents: Vec<String> = connected.iter().map(|(agent, _)| agent.0.clone()).collect();
    let rows: Vec<TakenRow> = sqlx::query_as(TAKE_SQL)
        .bind(agents)
        .bind(lease.as_secs_f64())
        .bind(skip)
        .bind(PER_AGENT_LIMIT)
        .bind(PASS_LIMIT)
        .fetch_all(&mut *tx)
        .await?;
    let taken = rows
        .into_iter()
        .map(
            |(tenant, id, agent, signed_job, attempts, prior_mark)| Taken {
                tenant,
                id,
                agent: AgentId(agent),
                signed_job,
                attempts,
                prior_mark,
            },
        )
        .collect();
    Ok((tx, taken))
}

/// The second half of a pass: marks what was queued, ends what could not be, commits. The marks
/// the database gave are added to `marks` as they come; the caller keeps them whether or not the
/// commit succeeded, and records which.
async fn mark(
    mut tx: sqlx::Transaction<'static, sqlx::Postgres>,
    dispatched: &Dispatched,
    marks: &mut Vec<(String, i32, String)>,
) -> Result<(), sqlx::Error> {
    for (job, _) in &dispatched.sent {
        let stamp: Option<(String,)> = sqlx::query_as(MARK_SQL)
            .bind(&job.tenant)
            .bind(&job.id)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some((stamp,)) = stamp {
            marks.push((job.id.clone(), job.attempts + 1, stamp));
        }
    }
    for job in &dispatched.too_large {
        tracing::warn!(job_id = %job.id, "a job's instruction is over the frame limit; the job is ended");
        sqlx::query(TOO_LARGE_SQL)
            .bind(&job.tenant)
            .bind(&job.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

/// Writes the answers and releases of one iteration in one transaction. Each statement is
/// conditional on the job and on the marks of the delivery it is about, so an answer for a job that
/// is finished or delivered, or that another delivery has marked since, writes nothing.
async fn write(pool: &PgPool, step: &Step) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    statement_timeouts(&mut tx).await?;
    let conn: &mut PgConnection = &mut tx;
    for flight in &step.delivered {
        sqlx::query(DELIVERED_SQL)
            .bind(&flight.tenant)
            .bind(&flight.id)
            .bind(flight.attempts)
            .bind(&flight.prior)
            .bind(&flight.returned)
            .bind(!flight.committed)
            .execute(&mut *conn)
            .await?;
    }
    for (flight, reason) in &step.rejected {
        sqlx::query(REJECTED_SQL)
            .bind(&flight.tenant)
            .bind(&flight.id)
            .bind(reason)
            .bind(flight.attempts)
            .bind(&flight.prior)
            .bind(&flight.returned)
            .bind(!flight.committed)
            .execute(&mut *conn)
            .await?;
    }
    for flight in &step.released {
        sqlx::query(RELEASE_SQL)
            .bind(&flight.tenant)
            .bind(&flight.id)
            .bind(flight.attempts)
            .bind(&flight.prior)
            .bind(&flight.returned)
            .bind(!flight.committed)
            .execute(&mut *conn)
            .await?;
    }
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use shared_proto::{AgentEventKind, AgentEventMessage, InstructionRejectReason};

    use super::*;
    use crate::hub::{OUTBOUND_CAPACITY, Registration};

    fn agent(name: &str) -> AgentId {
        AgentId(name.to_owned())
    }

    fn job(agent_name: &str, n: usize) -> Taken {
        Taken {
            tenant: "t".to_owned(),
            id: format!("job-{agent_name}-{n}"),
            agent: agent(agent_name),
            signed_job: vec![n as u8; 4],
            attempts: 0,
            prior_mark: None,
        }
    }

    fn ids(sent: &[(Taken, Delivery)]) -> Vec<String> {
        sent.iter().map(|(job, _)| job.id.clone()).collect()
    }

    fn at() -> std::time::Instant {
        std::time::Instant::now()
    }

    const LEASE: Duration = Duration::from_secs(90);

    fn inbound(agent: &AgentId, connection: ConnectionId, message: AgentMessage) -> Inbound {
        Inbound {
            agent: agent.clone(),
            connection,
            message,
        }
    }

    fn ack(agent: &AgentId, d: &Delivery) -> Inbound {
        inbound(
            agent,
            d.connection,
            AgentMessage::CommandAck {
                command_id: d.command_id.clone(),
            },
        )
    }

    /// One job queued for `a` on its connection; the flight is registered.
    fn one_flight(hub: &Hub, queue: &mut Queue, a: &AgentId) -> (Registration, Delivery) {
        let reg = hub.register(a).unwrap();
        let out = dispatch(hub, vec![job(&a.0, 1)], || false);
        queue.register(&out.sent, at());
        let delivery = out.sent[0].1.clone();
        (reg, delivery)
    }

    // Case 3
    #[test]
    fn a_job_for_an_agent_that_is_not_connected_or_is_closing_is_neither_marked_nor_remembered() {
        let hub = Hub::new();
        let mut queue = Queue::default();
        let out = dispatch(&hub, vec![job("a", 1)], || false);
        assert!(out.sent.is_empty() && out.too_large.is_empty());
        let _reg = hub.register(&agent("b")).unwrap();
        assert!(hub.close_agent(&agent("b"), 4002));
        let out = dispatch(&hub, vec![job("b", 1)], || false);
        assert!(out.sent.is_empty() && out.too_large.is_empty());
        queue.register(&out.sent, at());
        assert!(queue.in_flight().is_empty());
        // The stop has begun: no connection takes a frame.
        let _reg = hub.register(&agent("c")).unwrap();
        let _tasks = hub.begin_stop();
        assert!(dispatch(&hub, vec![job("c", 1)], || false).sent.is_empty());
    }

    // Case 4
    #[test]
    fn a_full_queue_leaves_that_job_and_the_agents_later_ones_for_a_later_pass_in_order() {
        let hub = Hub::new();
        let (a, b) = (agent("a"), agent("b"));
        let mut reg_a = hub.register(&a).unwrap();
        let _reg_b = hub.register(&b).unwrap();
        let mut taken: Vec<Taken> = (0..OUTBOUND_CAPACITY + 3).map(|n| job("a", n)).collect();
        taken.push(job("b", 0));
        let out = dispatch(&hub, taken, || false);
        let sent = ids(&out.sent);
        assert_eq!(sent.len(), OUTBOUND_CAPACITY + 1);
        assert_eq!(
            sent[OUTBOUND_CAPACITY], "job-b-0",
            "the other agent is served"
        );
        assert!(!sent.contains(&format!("job-a-{OUTBOUND_CAPACITY}")));
        // The agent's queue drains; the rest goes in created order.
        while reg_a.outbound.try_recv().is_ok() {}
        let later: Vec<Taken> = (OUTBOUND_CAPACITY..OUTBOUND_CAPACITY + 3)
            .map(|n| job("a", n))
            .collect();
        let out = dispatch(&hub, later, || false);
        assert_eq!(
            ids(&out.sent),
            [16, 17, 18].map(|n| format!("job-a-{n}")).to_vec()
        );
    }

    // Case 5
    #[test]
    fn a_frame_over_the_limit_is_ended_not_retried_and_does_not_block_the_agent() {
        let hub = Hub::new();
        let a = agent("a");
        let _reg = hub.register(&a).unwrap();
        let mut big = job("a", 0);
        big.signed_job = vec![0; shared_proto::MAX_CONTROL_MESSAGE_LEN];
        let out = dispatch(&hub, vec![big, job("a", 1)], || false);
        assert_eq!(out.too_large.len(), 1);
        assert_eq!(out.too_large[0].id, "job-a-0");
        assert_eq!(ids(&out.sent), ["job-a-1"]);
    }

    // The stop is asked before each send (20b's pure half)
    #[test]
    fn a_stop_between_two_sends_leaves_the_second_job_alone() {
        let hub = Hub::new();
        let _reg = hub.register(&agent("a")).unwrap();
        let asked = std::cell::Cell::new(0);
        let out = dispatch(&hub, vec![job("a", 0), job("a", 1)], || {
            asked.set(asked.get() + 1);
            asked.get() > 1
        });
        assert_eq!(ids(&out.sent), ["job-a-0"]);
    }

    // Case 8
    #[test]
    fn a_flight_stays_when_the_commit_failed_and_its_answer_still_settles_the_attempt() {
        let hub = Hub::new();
        let a = agent("a");
        let mut queue = Queue::default();
        let (_reg, delivery) = one_flight(&hub, &mut queue, &a);
        // The mark's commit failed: nothing is written, but the job is not taken again here.
        assert_eq!(queue.in_flight(), ["job-a-1"]);
        let snapshot = hub.connected();
        let step = queue.step(&snapshot, vec![ack(&a, &delivery)], at(), LEASE);
        assert_eq!(step.delivered.len(), 1);
        assert_eq!(step.delivered[0].attempts, 1);
        assert!(step.released.is_empty() && step.rejected.is_empty());
        assert!(queue.in_flight().is_empty());
    }

    // Case 12
    #[test]
    fn an_answer_read_before_the_connection_left_is_applied_and_nothing_is_released() {
        let hub = Hub::new();
        let a = agent("a");
        let mut queue = Queue::default();
        let (reg, delivery) = one_flight(&hub, &mut queue, &a);
        // The snapshot still has the connection, the ack is in the inbox, and the connection
        // leaves before the next iteration's snapshot.
        let snapshot = hub.connected();
        let step = queue.step(&snapshot, vec![ack(&a, &delivery)], at(), LEASE);
        assert_eq!(step.delivered.len(), 1);
        assert!(hub.leave(&a, reg.id));
        let step = queue.step(&hub.connected(), vec![], at(), LEASE);
        assert!(
            step.is_empty(),
            "the flight was settled before the departure"
        );
    }

    #[test]
    fn an_answer_read_after_the_snapshot_by_a_replaced_connection_costs_one_duplicate() {
        let hub = Hub::new();
        let a = agent("a");
        let mut queue = Queue::default();
        let (_old, delivery) = one_flight(&hub, &mut queue, &a);
        // The replacement registers; the iteration's snapshot has it, not the old connection.
        let new = hub.register(&a).unwrap();
        let snapshot = hub.connected();
        let step = queue.step(&snapshot, vec![], at(), LEASE);
        assert_eq!(step.released.len(), 1, "the unsettled delivery is released");
        // The old connection's ack arrives after that and matches nothing.
        let step = queue.step(&snapshot, vec![ack(&a, &delivery)], at(), LEASE);
        assert!(step.is_empty());
        assert_ne!(delivery.connection, new.id);
    }

    // Case 13
    #[test]
    fn an_unknown_repeated_or_foreign_answer_is_ignored() {
        let hub = Hub::new();
        let (a, b) = (agent("a"), agent("b"));
        let mut queue = Queue::default();
        let (_reg_a, delivery) = one_flight(&hub, &mut queue, &a);
        let _reg_b = hub.register(&b).unwrap();
        let snapshot = hub.connected();
        let unknown = inbound(
            &a,
            delivery.connection,
            AgentMessage::CommandAck {
                command_id: "c-999".to_owned(),
            },
        );
        let foreign = ack(&b, &delivery);
        let step = queue.step(&snapshot, vec![unknown, foreign], at(), LEASE);
        assert!(step.is_empty());
        assert_eq!(queue.in_flight().len(), 1, "the flight is untouched");
        let step = queue.step(
            &snapshot,
            vec![ack(&a, &delivery), ack(&a, &delivery)],
            at(),
            LEASE,
        );
        assert_eq!(step.delivered.len(), 1, "the repeat settles nothing");
    }

    #[test]
    fn a_rejection_settles_with_the_agents_reason_and_an_old_flight_is_forgotten_unreleased() {
        let hub = Hub::new();
        let a = agent("a");
        let mut queue = Queue::default();
        let (_reg, delivery) = one_flight(&hub, &mut queue, &a);
        let snapshot = hub.connected();
        let rejected = inbound(
            &a,
            delivery.connection,
            AgentMessage::CommandRejected {
                command_id: delivery.command_id.clone(),
                reason: InstructionRejectReason::BadSignature,
            },
        );
        let step = queue.step(&snapshot, vec![rejected], at(), LEASE);
        assert_eq!(step.rejected.len(), 1);
        assert_eq!(step.rejected[0].1, "badSignature");
        // A flight past the lease is forgotten without a release (its mark expires on its own).
        let (_reg2, _) = one_flight(&hub, &mut queue, &agent("c"));
        let later = at() + LEASE;
        let step = queue.step(&hub.connected(), vec![], later, LEASE);
        assert!(step.is_empty());
        assert!(queue.in_flight().is_empty());
    }

    // The inbox is read while the database is waited for
    #[tokio::test]
    async fn the_inbox_is_read_while_the_database_is_waited_for_and_what_came_is_kept() {
        let (sender, mut inbox) = mpsc::channel::<Inbound>(8);
        let (release, wait) = tokio::sync::oneshot::channel::<u32>();
        let a = agent("a");
        let connection = Hub::new().register(&a).unwrap().id;
        let producer = tokio::spawn(async move {
            // More than the inbox holds: a send finishes only if the consumer reads.
            for n in 0..40 {
                let message = inbound(
                    &a,
                    connection,
                    AgentMessage::CommandAck {
                        command_id: format!("c-{n}"),
                    },
                );
                sender.send(message).await.unwrap();
            }
            release.send(7).unwrap();
        });
        let mut held = Vec::new();
        // The stalled "database" ends only after the producer has sent all of its messages.
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            draining(&mut inbox, &mut held, async { wait.await.unwrap() }),
        )
        .await
        .expect("the inbox is read while the work waits");
        producer.await.unwrap();
        assert_eq!(out, 7);
        // What is still in the inbox when the work ends is read by the next iteration.
        while let Ok(item) = inbox.try_recv() {
            held.push(item);
        }
        assert_eq!(held.len(), 40, "nothing is lost, and the order is kept");
        assert!(matches!(
            &held[39].message,
            AgentMessage::CommandAck { command_id } if command_id == "c-39"
        ));
    }

    #[tokio::test]
    async fn the_buffer_is_bounded_so_that_a_stalled_database_still_pushes_back() {
        let (sender, mut inbox) = mpsc::channel::<Inbound>(4);
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let a = agent("a");
        let connection = Hub::new().register(&a).unwrap().id;
        let message = move || {
            inbound(
                &a,
                connection,
                AgentMessage::Reconcile { open_jobs: vec![] },
            )
        };
        let producer = tokio::spawn(async move {
            // The buffer takes HELD_LIMIT and the inbox 4 more; then a send finds it full.
            for _ in 0..HELD_LIMIT + 4 {
                sender.send(message()).await.unwrap();
            }
            tokio::task::yield_now().await;
            assert!(
                sender.try_send(message()).is_err(),
                "the inbox is full again"
            );
            release.send(()).unwrap();
        });
        let mut held = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            draining(&mut inbox, &mut held, async { wait.await.unwrap() }),
        )
        .await
        .expect("the work ends once the producer releases it");
        producer.await.unwrap();
        assert_eq!(held.len(), HELD_LIMIT);
    }

    #[test]
    fn a_step_that_was_not_written_is_kept_and_its_jobs_stay_out_of_the_take() {
        let hub = Hub::new();
        let a = agent("a");
        let mut queue = Queue::default();
        let (_reg, delivery) = one_flight(&hub, &mut queue, &a);
        let mut carry = Step::default();
        carry.append(queue.step(&hub.connected(), vec![ack(&a, &delivery)], at(), LEASE));
        // The write failed: the flight is gone from the queue, the step still names the job.
        assert!(queue.in_flight().is_empty());
        assert_eq!(carry.ids().collect::<Vec<_>>(), ["job-a-1"]);
        carry.append(Step::default());
        assert_eq!(carry.delivered.len(), 1);
    }

    #[test]
    fn after_a_commit_only_the_mark_the_database_gave_is_kept_in_the_flight() {
        let hub = Hub::new();
        let a = agent("a");
        let mut queue = Queue::default();
        let (_reg, delivery) = one_flight(&hub, &mut queue, &a);
        queue.set_marks(&[("job-a-1".to_owned(), 1, "m".to_owned())], true);
        let step = queue.step(&hub.connected(), vec![ack(&a, &delivery)], at(), LEASE);
        let flight = &step.delivered[0];
        assert_eq!(flight.returned.as_deref(), Some("m"));
        assert!(flight.committed, "the prior mark no longer counts");
    }

    #[test]
    fn a_flight_whose_mark_commit_failed_keeps_both_tokens_and_each_answer_settles_it() {
        let hub = Hub::new();
        let a = agent("a");
        let mut queue = Queue::default();
        let _reg = hub.register(&a).unwrap();
        let mut taken = job("a", 1);
        taken.attempts = 2;
        taken.prior_mark = Some("p".to_owned());
        let out = dispatch(&hub, vec![taken], || false);
        queue.register(&out.sent, at());
        let delivery = out.sent[0].1.clone();
        // The mark returned a token, and then the commit reported a failure.
        queue.set_marks(&[("job-a-1".to_owned(), 3, "m".to_owned())], false);
        let reject = inbound(
            &a,
            delivery.connection,
            AgentMessage::CommandRejected {
                command_id: delivery.command_id.clone(),
                reason: InstructionRejectReason::BadSignature,
            },
        );
        let kept = queue.flights.clone();
        let step = queue.step(&hub.connected(), vec![ack(&a, &delivery)], at(), LEASE);
        let flight = &step.delivered[0];
        assert_eq!(flight.attempts, 3);
        assert_eq!(flight.prior.as_deref(), Some("p"));
        assert_eq!(flight.returned.as_deref(), Some("m"));
        assert!(!flight.committed, "the prior mark still counts");
        // A rejection settles it with the same tokens.
        queue.flights = kept.clone();
        let step = queue.step(&hub.connected(), vec![reject], at(), LEASE);
        assert_eq!(step.rejected.len(), 1);
        assert_eq!(step.rejected[0].0, *flight);
        // So does the release when the connection leaves.
        queue.flights = kept;
        let step = queue.step(&[], vec![], at(), LEASE);
        assert_eq!(step.released.len(), 1);
        assert_eq!(step.released[0], *flight);
    }

    // Case 22
    #[test]
    fn events_and_reconciliations_are_dropped_and_settle_nothing() {
        let hub = Hub::new();
        let a = agent("a");
        let mut queue = Queue::default();
        let (_reg, delivery) = one_flight(&hub, &mut queue, &a);
        let event = AgentMessage::AgentEvent(AgentEventMessage {
            seq: None,
            occurred_at_agent: None,
            event: AgentEventKind::Progress {
                job_id: shared_proto::JobId("j".to_owned()),
                progress_permille: 1,
                phase: "run".to_owned(),
            },
        });
        let reconcile = AgentMessage::Reconcile { open_jobs: vec![] };
        let step = queue.step(
            &hub.connected(),
            vec![
                inbound(&a, delivery.connection, event),
                inbound(&a, delivery.connection, reconcile),
            ],
            at(),
            LEASE,
        );
        assert!(step.is_empty());
        assert_eq!(queue.in_flight().len(), 1);
    }
}

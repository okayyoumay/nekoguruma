//! The agent's guaranteed events at the server (design 5.3, 5.4; ADR-478 item 6, ADR-479): which
//! of the events an agent sent are stored, and the one transaction that stores them.
//!
//! [`Plan`] is the pure rule: an agent's cursor is the highest number stored, a number at or below
//! it is a repeat, the next one is stored, and anything else is a gap. [`store`] runs the rule for
//! one agent in one transaction: it finds the agent's tenant (an unrevoked row), makes sure the
//! cursor row exists, locks it (`FOR UPDATE`, so that two server instances serve one agent in
//! turn), inserts the events in order, compares each repeat's text with the stored row in SQL, moves
//! the cursor and commits. What the caller may tell the agent (an acknowledgement, a close) is
//! returned only when the commit went through.
//!
//! The row's `body` is the exact text of the agent's frame. `kind`, `job_id` and
//! `occurred_at_agent` are read out of the frame; the job ID only when it is a canonical UUID, the
//! device time only when [`strict_device_time`] accepts it, so that no value of the frame can make
//! the insert fail for good.
//!
//! A stored `jobState` event is then applied to its job (design 5.6, ADR-481): in the same
//! transaction, under a lock on the sender's own job row, [`crate::job_state::decide`] picks one
//! write, and the one that ends the job deletes the job's own vehicle lock in the same statement. A
//! report that is not applied is stored and acknowledged all the same.
//!
//! Never logged: the text of an event.

use shared_proto::{AgentEventKind, AgentId, AgentMessage, JobState, is_canonical_uuid};
use sqlx::PgPool;

use crate::db::statement_timeouts;
use crate::hub::{ConnectionId, Inbound};
use crate::job_state::{Current, Decision, Report, decide};

/// A numbered event of one agent, as the queue keeps it until it is stored.
#[derive(Debug, Clone)]
pub(crate) struct Event {
    pub(crate) agent: AgentId,
    /// The connection that read it; its acknowledgement goes there.
    pub(crate) connection: ConnectionId,
    pub(crate) seq: u64,
    pub(crate) kind: &'static str,
    /// The job the event names, when that is a canonical UUID.
    pub(crate) job_id: Option<String>,
    /// The device time, when it passed [`strict_device_time`].
    pub(crate) occurred_at: Option<String>,
    /// The exact text of the frame.
    pub(crate) text: String,
    /// What a `jobState` event reports (design 5.6); none for any other kind.
    pub(crate) report: Option<Report>,
}

impl Event {
    /// The event of a message that is a numbered `agentEvent` with its text; anything else comes
    /// back unchanged (boxed, as the error of a result).
    pub(crate) fn of(inbound: Inbound) -> Result<Event, Box<Inbound>> {
        let AgentMessage::AgentEvent(event) = &inbound.message else {
            return Err(Box::new(inbound));
        };
        let (Some(seq), Some(_)) = (event.seq, &inbound.text) else {
            return Err(Box::new(inbound));
        };
        let (kind, job_id, report) = match &event.event {
            AgentEventKind::JobState {
                job_id,
                state,
                failure_code,
                procedure_code,
                detail,
            } => (
                "jobState",
                &job_id.0,
                Some(Report::of(
                    state,
                    failure_code.as_deref(),
                    procedure_code.as_deref(),
                    detail.as_deref(),
                )),
            ),
            AgentEventKind::HmiRequest { job_id, .. } => ("hmiRequest", &job_id.0, None),
            // The codec refuses a number on `progress`; it is never stored.
            AgentEventKind::Progress { .. } => return Err(Box::new(inbound)),
        };
        let job_id = is_canonical_uuid(job_id).then(|| job_id.clone());
        let occurred_at = event
            .occurred_at_agent
            .as_deref()
            .and_then(strict_device_time)
            .map(str::to_owned);
        let text = inbound.text.expect("checked above");
        Ok(Event {
            agent: inbound.agent,
            connection: inbound.connection,
            seq,
            kind,
            job_id,
            occurred_at,
            text,
            report,
        })
    }
}

/// `text` when it is a date-time that PostgreSQL's `timestamptz` input is known to read the way
/// the agent means it: `YYYY-MM-DDTHH:MM:SS`, optionally a fraction of one to nine digits, and `Z`
/// or an offset `+HH:MM` / `-HH:MM`; a real calendar date, a year from 0001, an offset of at most
/// 15:59 (what the database accepts). Anything looser is not trusted, and the column stays null.
pub(crate) fn strict_device_time(text: &str) -> Option<&str> {
    let b = text.as_bytes();
    let digits = |from: usize, len: usize| -> Option<u32> {
        let part = b.get(from..from + len)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| part.iter().fold(0, |n, d| n * 10 + u32::from(d - b'0')))
    };
    let at = |i: usize, c: u8| b.get(i) == Some(&c);
    let (year, month, day) = (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?);
    if !(at(4, b'-') && at(7, b'-') && at(10, b'T') && at(13, b':') && at(16, b':')) {
        return None;
    }
    let (hour, minute, second) = (digits(11, 2)?, digits(14, 2)?, digits(17, 2)?);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if year < 1 || day < 1 || day > days || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut i = 19;
    if at(i, b'.') {
        let start = i + 1;
        i = start;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if !(1..=9).contains(&(i - start)) {
            return None;
        }
    }
    match b.get(i)? {
        b'Z' => (i + 1 == b.len()).then_some(text),
        b'+' | b'-' => {
            let (hours, minutes) = (digits(i + 1, 2)?, digits(i + 4, 2)?);
            let ok = at(i + 3, b':') && i + 6 == b.len() && hours <= 15 && minutes <= 59;
            ok.then_some(text)
        }
        _ => None,
    }
}

/// What the rule says about one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The number is the next one: store it.
    Store,
    /// At or below the cursor: not stored; the stored row's text decides what it means.
    Repeat,
    /// Above the next number, or a number no `bigint` holds: not stored, and the connection that
    /// sent it closes.
    Gap,
    /// An earlier event of the same connection ended its standing (a gap, or a repeat whose text
    /// differs): not looked at.
    Ignored,
}

/// The rule for one agent, applied to its events in the order they were read. It keeps the highest
/// number stored so far in the run, so a number stored earlier in the same run makes a later copy
/// of it a repeat.
#[derive(Debug)]
pub(crate) struct Plan {
    last: u64,
    ended: Vec<ConnectionId>,
}

impl Plan {
    /// A plan for an agent whose cursor is `cursor`.
    pub(crate) fn new(cursor: i64) -> Plan {
        Plan {
            last: u64::try_from(cursor).unwrap_or(0),
            ended: Vec::new(),
        }
    }

    /// The highest number stored so far, the cursor if the run goes on to be committed.
    pub(crate) fn last(&self) -> u64 {
        self.last
    }

    /// The verdict for the next event, `seq` read on `connection`. A [`Verdict::Store`] counts as
    /// stored from here on.
    pub(crate) fn next(&mut self, connection: ConnectionId, seq: u64) -> Verdict {
        if self.ended.contains(&connection) {
            return Verdict::Ignored;
        }
        if seq <= self.last {
            return Verdict::Repeat;
        }
        if seq == self.last + 1 && i64::try_from(seq).is_ok() {
            self.last = seq;
            return Verdict::Store;
        }
        self.ended.push(connection);
        Verdict::Gap
    }

    /// Ends `connection`'s standing: its later events are [`Verdict::Ignored`]. Called when its
    /// repeat differs from the stored row.
    pub(crate) fn end(&mut self, connection: ConnectionId) {
        if !self.ended.contains(&connection) {
            self.ended.push(connection);
        }
    }
}

/// Why a connection is to be closed with 1002.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cause {
    /// The number is not the next one.
    Gap,
    /// A repeat whose text differs from the stored one.
    Differs,
}

/// What a committed [`store`] lets the caller do.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Stored {
    /// The highest number to acknowledge on each connection: the event's own when it was stored,
    /// the cursor when it was a repeat of a stored or deleted row.
    pub(crate) acks: Vec<(ConnectionId, u64)>,
    /// The connections to close, with the number of the event that decided it.
    pub(crate) closes: Vec<(ConnectionId, u64, Cause)>,
    pub(crate) stored: u64,
    pub(crate) repeats: u64,
    /// The stored events that wrote to their job (design 5.6, ADR-481).
    pub(crate) applied: u64,
    /// The stored `jobState` events that wrote nothing: the job is not the sender's or does not
    /// exist (a job ID that is not canonical included), is finished, or the report is not a state.
    pub(crate) ignored: u64,
    /// The reports not applied that the caller logs, after the commit.
    pub(crate) unapplied: Vec<Unapplied>,
}

/// A report that was stored and not applied to its job, as the caller logs it: names only, never
/// the text.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unapplied {
    pub(crate) seq: u64,
    /// The job the report names, when that is a canonical UUID.
    pub(crate) job_id: Option<String>,
    /// The job's state, or `unknown` for a name that is not a state or a job that is not found.
    pub(crate) current: &'static str,
    pub(crate) reported: &'static str,
}

impl Stored {
    /// A report that wrote nothing and is to be logged.
    fn not_applied(&mut self, seq: u64, job: Option<&str>, current: &'static str, report: &Report) {
        self.ignored += 1;
        self.unapplied.push(Unapplied {
            seq,
            job_id: job.map(str::to_owned),
            current,
            reported: report.name(),
        });
    }

    fn ack(&mut self, connection: ConnectionId, seq: u64) {
        match self.acks.iter_mut().find(|(c, _)| *c == connection) {
            Some((_, highest)) => *highest = (*highest).max(seq),
            None => self.acks.push((connection, seq)),
        }
    }
}

const TENANT_SQL: &str =
    "SELECT tenant_id::text FROM agents WHERE id = $1::uuid AND revoked_at IS NULL";
const CURSOR_ROW_SQL: &str = "INSERT INTO agent_event_cursor (tenant_id, agent_id) \
     VALUES ($1::uuid, $2::uuid) ON CONFLICT DO NOTHING";
const CURSOR_LOCK_SQL: &str = "SELECT last_seq FROM agent_event_cursor \
     WHERE tenant_id = $1::uuid AND agent_id = $2::uuid FOR UPDATE";
const INSERT_SQL: &str = "INSERT INTO agent_events \
     (tenant_id, agent_id, seq, kind, job_id, body, occurred_at_agent) \
     VALUES ($1::uuid, $2::uuid, $3, $4, $5::uuid, $6, $7::timestamptz)";
/// Whether the stored row's text equals `$4`; no row when it was deleted.
const COMPARE_SQL: &str = "SELECT body = $4 FROM agent_events \
     WHERE tenant_id = $1::uuid AND agent_id = $2::uuid AND seq = $3";
/// The sender's own job, locked: `$3` is the sender, so another agent's job is no row.
const JOB_LOCK_SQL: &str = "SELECT state, finished_at IS NOT NULL FROM jobs \
     WHERE tenant_id = $1::uuid AND id = $2::uuid AND agent_id = $3::uuid FOR UPDATE";
/// `received`: the stamps, never the state. The times are the transaction's, not the device's
/// (design 16.3).
const DELIVERED_SQL: &str = "UPDATE jobs SET last_reported_at = now(), \
     delivered_at = coalesce(delivered_at, now()) \
     WHERE tenant_id = $1::uuid AND id = $2::uuid";
const TOUCH_SQL: &str = "UPDATE jobs SET last_reported_at = now() \
     WHERE tenant_id = $1::uuid AND id = $2::uuid";
/// `$3` the state. A job that an agent's needsOnSiteRecovery had ended is open again: no end, no
/// failure. It takes no lock again.
const MOVE_SQL: &str = "UPDATE jobs SET state = $3, finished_at = NULL, failure_code = NULL, \
     procedure_code = NULL, failure_detail = NULL, last_reported_at = now(), \
     delivered_at = coalesce(delivered_at, now()) \
     WHERE tenant_id = $1::uuid AND id = $2::uuid";
/// `$3` the state, `$4` the failure code, `$5` the procedure code, `$6` the detail. The job's own
/// vehicle lock goes in the same statement (design 8.8, ADR-480); a lock another job took after
/// this one's expired stays.
const END_SQL: &str = "WITH ended AS ( \
       UPDATE jobs SET state = $3, failure_code = $4, procedure_code = $5, failure_detail = $6, \
           finished_at = now(), last_reported_at = now(), \
           delivered_at = coalesce(delivered_at, now()) \
       WHERE tenant_id = $1::uuid AND id = $2::uuid \
       RETURNING tenant_id, id, vin) \
     DELETE FROM vehicle_locks l USING ended e \
     WHERE l.tenant_id = e.tenant_id AND l.vin = e.vin AND l.job_id = e.id";
const CURSOR_MOVE_SQL: &str = "UPDATE agent_event_cursor SET last_seq = $3, updated_at = now() \
     WHERE tenant_id = $1::uuid AND agent_id = $2::uuid";

/// Stores the events of one agent, which are in the order they were read, in one transaction. An
/// agent with no unrevoked row gets nothing stored and nothing answered. A failure leaves nothing
/// stored (a commit reported as failed may have gone through; the caller sends the events again, and
/// they are then repeats of stored rows with the same text).
pub(crate) async fn store(
    pool: &PgPool,
    agent: &AgentId,
    events: &[Event],
) -> Result<Stored, sqlx::Error> {
    let mut tx = pool.begin().await?;
    statement_timeouts(&mut tx).await?;
    let tenant: Option<(String,)> = sqlx::query_as(TENANT_SQL)
        .bind(&agent.0)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((tenant,)) = tenant else {
        return Ok(Stored::default());
    };
    sqlx::query(CURSOR_ROW_SQL)
        .bind(&tenant)
        .bind(&agent.0)
        .execute(&mut *tx)
        .await?;
    let (cursor,): (i64,) = sqlx::query_as(CURSOR_LOCK_SQL)
        .bind(&tenant)
        .bind(&agent.0)
        .fetch_one(&mut *tx)
        .await?;
    let mut plan = Plan::new(cursor);
    let mut out = Stored::default();
    for event in events {
        // A number no `bigint` holds is a gap, and never reaches the statements.
        let seq = i64::try_from(event.seq).unwrap_or(i64::MAX);
        match plan.next(event.connection, event.seq) {
            Verdict::Ignored => {}
            Verdict::Gap => out.closes.push((event.connection, event.seq, Cause::Gap)),
            Verdict::Store => {
                sqlx::query(INSERT_SQL)
                    .bind(&tenant)
                    .bind(&agent.0)
                    .bind(seq)
                    .bind(event.kind)
                    .bind(&event.job_id)
                    .bind(&event.text)
                    .bind(&event.occurred_at)
                    .execute(&mut *tx)
                    .await?;
                out.stored += 1;
                out.ack(event.connection, event.seq);
                match (&event.job_id, &event.report) {
                    (Some(job), Some(report)) => {
                        apply(&mut tx, &tenant, agent, job, event.seq, report, &mut out).await?;
                    }
                    // A job ID that is not canonical names no job.
                    (None, Some(report)) => out.not_applied(event.seq, None, "unknown", report),
                    _ => {}
                }
            }
            Verdict::Repeat => {
                let same: Option<(bool,)> = sqlx::query_as(COMPARE_SQL)
                    .bind(&tenant)
                    .bind(&agent.0)
                    .bind(seq)
                    .bind(&event.text)
                    .fetch_optional(&mut *tx)
                    .await?;
                // A deleted row (no row) is answered with the cursor like a same one.
                if same.is_none_or(|(same,)| same) {
                    out.repeats += 1;
                    out.ack(event.connection, plan.last());
                } else {
                    plan.end(event.connection);
                    out.closes
                        .push((event.connection, event.seq, Cause::Differs));
                }
            }
        }
    }
    if out.stored > 0 {
        sqlx::query(CURSOR_MOVE_SQL)
            .bind(&tenant)
            .bind(&agent.0)
            .bind(i64::try_from(plan.last()).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(out)
}

/// Applies a stored report to its job in the transaction of [`store`].
async fn apply(
    tx: &mut sqlx::PgConnection,
    tenant: &str,
    agent: &AgentId,
    job: &str,
    seq: u64,
    report: &Report,
    out: &mut Stored,
) -> Result<(), sqlx::Error> {
    let row: Option<(String, bool)> = sqlx::query_as(JOB_LOCK_SQL)
        .bind(tenant)
        .bind(job)
        .bind(&agent.0)
        .fetch_optional(&mut *tx)
        .await?;
    let current = row.map(|(state, finished)| Current { state, finished });
    let state = report.name();
    let query = match decide(current.as_ref(), report) {
        Decision::NoJob => {
            out.not_applied(seq, Some(job), "unknown", report);
            return Ok(());
        }
        Decision::Ignore { warn } => {
            if warn {
                let current = current.as_ref().map_or("unknown", |c| {
                    JobState::from_wire_name(&c.state).map_or("unknown", |s| s.wire_name())
                });
                out.not_applied(seq, Some(job), current, report);
            } else {
                out.ignored += 1;
            }
            return Ok(());
        }
        Decision::Delivered => sqlx::query(DELIVERED_SQL).bind(tenant).bind(job),
        Decision::Touch => sqlx::query(TOUCH_SQL).bind(tenant).bind(job),
        Decision::Move => sqlx::query(MOVE_SQL).bind(tenant).bind(job).bind(state),
        Decision::End => sqlx::query(END_SQL)
            .bind(tenant)
            .bind(job)
            .bind(state)
            .bind(report.failure_code)
            .bind(&report.procedure_code)
            .bind(&report.detail),
    };
    query.execute(&mut *tx).await?;
    out.applied += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use shared_proto::{AgentEventKind, AgentEventMessage, HmiRequestBody, JobId};

    use super::*;
    use crate::hub::Hub;

    fn connections(n: usize) -> Vec<ConnectionId> {
        let hub = Hub::new();
        (0..n)
            .map(|i| hub.register(&AgentId(format!("a{i}"))).unwrap().id)
            .collect()
    }

    fn run(cursor: i64, items: &[(usize, u64)]) -> Vec<Verdict> {
        let ids = connections(3);
        let mut plan = Plan::new(cursor);
        items
            .iter()
            .map(|(c, seq)| plan.next(ids[*c], *seq))
            .collect()
    }

    use Verdict::{Gap, Ignored, Repeat, Store};

    // Store case E1
    #[test]
    fn the_next_numbers_in_order_are_stored() {
        assert_eq!(run(0, &[(0, 1), (0, 2), (0, 3)]), [Store, Store, Store]);
        assert_eq!(run(7, &[(0, 8), (0, 9)]), [Store, Store]);
    }

    // Store case E2
    #[test]
    fn a_number_at_or_below_the_cursor_is_a_repeat() {
        assert_eq!(run(5, &[(0, 5), (0, 1), (0, 3)]), [Repeat, Repeat, Repeat]);
    }

    // Store case E5
    #[test]
    fn a_gap_ends_that_connections_later_events_and_not_its_siblings() {
        // Connection 0 skips from 3 to 5; connection 1's 4 is the next number.
        assert_eq!(
            run(3, &[(0, 5), (0, 4), (1, 4), (0, 6), (1, 5)]),
            [Gap, Ignored, Store, Ignored, Store]
        );
        // A number is not skipped over: 2 when the cursor is 0.
        assert_eq!(run(0, &[(0, 2), (0, 1)]), [Gap, Ignored]);
    }

    // Store case E6
    #[test]
    fn a_number_no_bigint_holds_is_a_gap_even_when_it_would_be_the_next() {
        let max = i64::MAX.unsigned_abs();
        assert_eq!(run(i64::MAX, &[(0, max + 1)]), [Gap]);
        assert_eq!(run(0, &[(0, u64::MAX)]), [Gap]);
        assert_eq!(run(0, &[(0, max)]), [Gap], "far above the next one");
        assert_eq!(run(i64::MAX - 1, &[(0, max)]), [Store]);
        assert_eq!(run(i64::MAX, &[(0, max)]), [Repeat]);
    }

    // Store case E7
    #[test]
    fn a_number_stored_earlier_in_the_run_makes_its_copy_a_repeat() {
        assert_eq!(
            run(0, &[(0, 1), (0, 1), (1, 1), (0, 2), (1, 2)]),
            [Store, Repeat, Repeat, Store, Repeat]
        );
    }

    #[test]
    fn a_connection_whose_repeat_differs_is_ended_and_the_others_are_not() {
        let ids = connections(2);
        let mut plan = Plan::new(2);
        assert_eq!(plan.next(ids[0], 1), Repeat);
        plan.end(ids[0]);
        assert_eq!(plan.next(ids[0], 3), Ignored);
        assert_eq!(plan.next(ids[1], 3), Store);
        assert_eq!(plan.last(), 3);
    }

    #[test]
    fn a_negative_cursor_is_read_as_none_stored() {
        assert_eq!(run(-1, &[(0, 1)]), [Store]);
    }

    // Store case E19 (the device time's half)
    #[test]
    fn a_device_time_is_kept_only_when_it_is_a_strict_date_time() {
        for good in [
            "2026-11-14T10:20:30Z",
            "2026-11-14T10:20:30.5Z",
            "2026-11-14T10:20:30.123456789+09:00",
            "2026-11-14T10:20:30-15:59",
            "2024-02-29T00:00:00Z",
            "0001-01-01T00:00:00Z",
            "9999-12-31T23:59:59Z",
        ] {
            assert_eq!(strict_device_time(good), Some(good), "{good}");
        }
        for bad in [
            "",
            "yesterday",
            "2026-11-14",
            "2026-11-14 10:20:30Z",
            "2026-11-14t10:20:30Z",
            "2026-11-14T10:20:30z",
            "2026-11-14T10:20:30",
            "2026-11-14T10:20:30.Z",
            "2026-11-14T10:20:30.1234567890Z",
            "2026-13-14T10:20:30Z",
            "2026-00-14T10:20:30Z",
            "2026-11-31T10:20:30Z",
            "2025-02-29T00:00:00Z",
            "2026-11-00T10:20:30Z",
            "0000-01-01T00:00:00Z",
            "2026-11-14T24:00:00Z",
            "2026-11-14T10:60:00Z",
            "2026-11-14T10:20:60Z",
            "2026-11-14T10:20:30+16:00",
            "2026-11-14T10:20:30+09:60",
            "2026-11-14T10:20:30+0900",
            "2026-11-14T10:20:30+09",
            "2026-11-14T10:20:30Z ",
            " 2026-11-14T10:20:30Z",
            "2026-11-14T10:20:30+09:00Z",
            "+026-11-14T10:20:30Z",
            "２０２６-11-14T10:20:30Z",
        ] {
            assert_eq!(strict_device_time(bad), None, "{bad:?}");
        }
    }

    fn inbound(message: AgentMessage, text: Option<&str>) -> Inbound {
        let hub = Hub::new();
        let agent = AgentId("a".to_owned());
        Inbound {
            connection: hub.register(&agent).unwrap().id,
            agent,
            message,
            text: text.map(str::to_owned),
        }
    }

    fn job_state(seq: Option<u64>, job: &str, at: Option<&str>) -> AgentMessage {
        AgentMessage::AgentEvent(AgentEventMessage {
            seq,
            occurred_at_agent: at.map(str::to_owned),
            event: AgentEventKind::JobState {
                job_id: JobId(job.to_owned()),
                state: "running".to_owned(),
                failure_code: None,
                procedure_code: None,
                detail: None,
            },
        })
    }

    const JOB: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f";

    // Store case E17 (the job ID's half), E19
    #[test]
    fn an_event_takes_its_kind_job_id_and_device_time_from_the_frame_only_when_they_are_well_formed()
     {
        let event = Event::of(inbound(
            job_state(Some(4), JOB, Some("2026-11-14T10:20:30Z")),
            Some("frame"),
        ))
        .unwrap();
        assert_eq!(
            (
                event.seq,
                event.kind,
                event.job_id.as_deref(),
                event.text.as_str()
            ),
            (4, "jobState", Some(JOB), "frame")
        );
        assert_eq!(event.occurred_at.as_deref(), Some("2026-11-14T10:20:30Z"));
        // Upper-case hexadecimal, no hyphens or a short text are not canonical: no job ID.
        for job in [
            "0194F0A2-7C3E-7D11-8A52-3F6F9B1C2D4F",
            "j",
            "",
            &JOB.replace('-', ""),
        ] {
            let event =
                Event::of(inbound(job_state(Some(1), job, Some("soon")), Some("f"))).unwrap();
            assert_eq!(event.job_id, None, "{job:?}");
            assert_eq!(event.occurred_at, None);
        }
        let hmi = AgentMessage::AgentEvent(AgentEventMessage {
            seq: Some(2),
            occurred_at_agent: None,
            event: AgentEventKind::HmiRequest {
                job_id: JobId(JOB.to_owned()),
                hmi_request: HmiRequestBody {
                    request_id: "r".to_owned(),
                    template_id: "t".to_owned(),
                },
            },
        });
        assert_eq!(
            Event::of(inbound(hmi, Some("f"))).unwrap().kind,
            "hmiRequest"
        );
    }

    // Store case E20
    #[test]
    fn what_is_not_a_numbered_event_with_its_text_is_not_an_event() {
        assert!(Event::of(inbound(job_state(None, JOB, None), Some("f"))).is_err());
        assert!(Event::of(inbound(job_state(Some(1), JOB, None), None)).is_err());
        let progress = AgentMessage::AgentEvent(AgentEventMessage {
            seq: Some(1),
            occurred_at_agent: None,
            event: AgentEventKind::Progress {
                job_id: JobId(JOB.to_owned()),
                progress_permille: 1,
                phase: "p".to_owned(),
            },
        });
        assert!(Event::of(inbound(progress, Some("f"))).is_err());
        let reconcile = AgentMessage::Reconcile { open_jobs: vec![] };
        assert!(Event::of(inbound(reconcile, Some("f"))).is_err());
    }
}

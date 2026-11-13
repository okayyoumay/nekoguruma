//! The revocation pass (design 14.4, ADR-466 item 5, ADR-468 item 11): every 30 seconds one task
//! of the hub's set reads which of the connected agents are still not revoked and closes the
//! others with 4002, after it has dropped their session tokens.
//!
//! The pass ends when the stop begins, in its wait for the next tick and in its read alike, since
//! `Server::run` waits for the hub's task set to empty. It reads only, so a failed or dropped read
//! decides nothing: nobody is closed on a read that failed.

use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use shared_proto::{AgentId, CLOSE_CODE_REVOKED};
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::health::may_log;
use crate::hub::Hub;
use crate::session::{Sessions, now};

/// The time between two passes. The first comes one period after the start.
pub(crate) const PERIOD: Duration = Duration::from_secs(30);
/// A failed read is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);

/// Resolves when the stop has begun (or the hub is gone).
async fn stopped(stopping: &mut watch::Receiver<bool>) {
    let _ = stopping.wait_for(|stop| *stop).await;
}

/// Runs the pass until the stop. `live` answers which of the given agent IDs have a row that is
/// not revoked.
pub(crate) async fn run<F, Fut>(
    hub: Hub,
    sessions: Sessions,
    mut stopping: watch::Receiver<bool>,
    mut live: F,
) where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<Vec<String>, sqlx::Error>>,
{
    // A tick missed because the clock jumped or the task was slow gives one pass, not a burst.
    let mut ticks = interval_at(Instant::now() + PERIOD, PERIOD);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let gate = Mutex::new(None);
    loop {
        tokio::select! {
            () = stopped(&mut stopping) => return,
            _ = ticks.tick() => {}
        }
        tokio::select! {
            () = stopped(&mut stopping) => return,
            () = pass(&hub, &sessions, &mut live, &gate) => {}
        }
    }
}

/// One pass: the agents connected now, minus the ones the read returns, are revoked.
async fn pass<F, Fut>(
    hub: &Hub,
    sessions: &Sessions,
    live: &mut F,
    gate: &Mutex<Option<std::time::Instant>>,
) where
    F: FnMut(Vec<String>) -> Fut,
    Fut: Future<Output = Result<Vec<String>, sqlx::Error>>,
{
    let snapshot = hub.connected();
    if snapshot.is_empty() {
        return;
    }
    let ids = snapshot.iter().map(|(agent, _)| agent.0.clone()).collect();
    let live = match live(ids).await {
        Ok(live) => live,
        Err(error) => {
            if may_log(gate, now(), LOG_EVERY) {
                let what = crate::db::failure(&error);
                tracing::warn!(
                    ?what,
                    "the revocation pass could not read the agents; closing nobody"
                );
            }
            return;
        }
    };
    // A connection that registered after the snapshot is not judged until the next pass.
    for (agent, _) in snapshot {
        if !live.contains(&agent.0) {
            revoke(hub, sessions, &agent);
        }
    }
}

/// Drops the agent's tokens, then closes its current connection, if it still has one.
fn revoke(hub: &Hub, sessions: &Sessions, agent: &AgentId) {
    sessions.revoke_agent(agent);
    hub.close_agent(agent, CLOSE_CODE_REVOKED);
    tracing::warn!(agent_id = %agent.0, "the agent is revoked or unknown; its control channel is closed");
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::hub::Registration;

    fn agent(name: &str) -> AgentId {
        AgentId(name.to_owned())
    }

    /// What the stand-in for the database answers: `None` is a failed read.
    type Answer = Arc<Mutex<Option<Vec<String>>>>;
    type Calls = Arc<Mutex<Vec<Vec<String>>>>;

    fn reads(
        answer: &Answer,
        calls: &Calls,
    ) -> impl FnMut(Vec<String>) -> std::future::Ready<Result<Vec<String>, sqlx::Error>> + use<>
    {
        let (answer, calls) = (answer.clone(), calls.clone());
        move |ids| {
            calls.lock().unwrap().push(ids);
            std::future::ready(
                answer
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or(sqlx::Error::PoolTimedOut),
            )
        }
    }

    fn answer(live: &[&str]) -> Answer {
        Arc::new(Mutex::new(Some(
            live.iter().map(|s| (*s).to_owned()).collect(),
        )))
    }

    fn closed(registration: &mut Registration) -> Option<u16> {
        registration.close.try_recv().ok()
    }

    /// Lets the spawned task run to its next wait.
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    struct Running {
        hub: Hub,
        sessions: Sessions,
        calls: Calls,
        answer: Answer,
        task: tokio::task::JoinHandle<()>,
    }

    /// Starts the pass and lets it reach its first wait, so that its clock starts now.
    async fn start(live: &[&str]) -> Running {
        let (hub, sessions) = (Hub::new(), Sessions::new());
        let (answer, calls) = (answer(live), Calls::default());
        let task = tokio::spawn(run(
            hub.clone(),
            sessions.clone(),
            hub.stopping(),
            reads(&answer, &calls),
        ));
        settle().await;
        Running {
            hub,
            sessions,
            calls,
            answer,
            task,
        }
    }

    // C1
    #[tokio::test(start_paused = true)]
    async fn the_first_pass_comes_after_one_period_and_a_long_jump_gives_one_pass() {
        let r = start(&["a"]).await;
        let mut a = r.hub.register(&agent("a")).unwrap();
        settle().await;
        assert!(r.calls.lock().unwrap().is_empty(), "none at the start");
        tokio::time::advance(PERIOD - Duration::from_secs(1)).await;
        settle().await;
        assert!(r.calls.lock().unwrap().is_empty(), "none before 30 s");
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(r.calls.lock().unwrap().len(), 1);
        assert_eq!(closed(&mut a), None, "a live agent is left alone");
        // Ten periods pass at once: one more pass, not ten.
        tokio::time::advance(PERIOD * 10).await;
        settle().await;
        assert_eq!(r.calls.lock().unwrap().len(), 2);
        r.task.abort();
    }

    // C2
    #[tokio::test(start_paused = true)]
    async fn nobody_connected_means_no_query() {
        let r = start(&[]).await;
        tokio::time::advance(PERIOD * 3).await;
        settle().await;
        assert!(r.calls.lock().unwrap().is_empty());
        // The marker: once an agent is connected the next pass does query.
        let _a = r.hub.register(&agent("a")).unwrap();
        tokio::time::advance(PERIOD).await;
        settle().await;
        assert_eq!(r.calls.lock().unwrap().len(), 1);
        r.task.abort();
    }

    // C4 (the hub's half), C5
    #[tokio::test(start_paused = true)]
    async fn a_revoked_or_missing_row_drops_the_tokens_and_closes_only_that_agent() {
        let r = start(&["keep"]).await;
        let mut revoked = r.hub.register(&agent("revoked")).unwrap();
        let mut missing = r.hub.register(&agent("missing")).unwrap();
        let mut keep = r.hub.register(&agent("keep")).unwrap();
        let token = r.sessions.mint(&agent("revoked"), now()).unwrap();
        let other = r.sessions.mint(&agent("keep"), now()).unwrap();
        tokio::time::advance(PERIOD).await;
        settle().await;
        assert_eq!(closed(&mut revoked), Some(4002));
        assert_eq!(closed(&mut missing), Some(4002));
        assert_eq!(closed(&mut keep), None);
        assert!(r.sessions.authenticate(&token).is_none());
        assert!(r.sessions.authenticate(&other).is_some());
        // The entries stay until the tasks leave.
        assert_eq!(r.hub.connected().len(), 3);
        r.task.abort();
    }

    // C6
    #[tokio::test(start_paused = true)]
    async fn a_failed_read_closes_nobody_and_the_next_tick_is_normal() {
        let r = start(&[]).await;
        let mut a = r.hub.register(&agent("a")).unwrap();
        let token = r.sessions.mint(&agent("a"), now()).unwrap();
        *r.answer.lock().unwrap() = None;
        tokio::time::advance(PERIOD).await;
        settle().await;
        assert_eq!(r.calls.lock().unwrap().len(), 1);
        assert_eq!(closed(&mut a), None);
        assert!(r.sessions.authenticate(&token).is_some());
        *r.answer.lock().unwrap() = Some(vec![]);
        tokio::time::advance(PERIOD).await;
        settle().await;
        assert_eq!(closed(&mut a), Some(4002));
        r.task.abort();
    }

    // C7, C8, C9, C10: the agents change while the read is in flight.
    #[tokio::test]
    async fn what_changes_after_the_snapshot_is_judged_by_what_the_agent_has_then() {
        let hub = Hub::new();
        let sessions = Sessions::new();
        let gate = Mutex::new(None);
        let (late, gone, replaced, told) = (
            agent("late"),
            agent("gone"),
            agent("replaced"),
            agent("told"),
        );
        let _gone_reg = hub.register(&gone).unwrap();
        let _replaced_old = hub.register(&replaced).unwrap();
        let mut told_reg = hub.register(&told).unwrap();
        let gone_token = sessions.mint(&gone, now()).unwrap();
        let late_token = sessions.mint(&late, now()).unwrap();
        let mut late_reg = None;
        let mut replaced_new = None;
        let mut read = |_ids: Vec<String>| {
            // Registered after the snapshot: not in it, so not judged.
            late_reg = Some(hub.register(&late).unwrap());
            // Left after the snapshot.
            assert!(hub.leave(
                &gone,
                hub.connected().iter().find(|(a, _)| *a == gone).unwrap().1
            ));
            // Replaced after the snapshot.
            replaced_new = Some(hub.register(&replaced).unwrap());
            // Already instructed.
            assert!(hub.close_agent(&told, 4001));
            std::future::ready(Ok(vec![]))
        };
        pass(&hub, &sessions, &mut read, &gate).await;
        assert!(late_reg.as_mut().unwrap().close.try_recv().is_err());
        assert!(sessions.authenticate(&late_token).is_some());
        assert!(
            sessions.authenticate(&gone_token).is_none(),
            "tokens go though it left"
        );
        assert_eq!(
            replaced_new.as_mut().unwrap().close.try_recv(),
            Ok(4002),
            "the newer connection is closed"
        );
        assert_eq!(told_reg.close.try_recv(), Ok(4001), "no second instruction");
    }

    // C11
    #[tokio::test(start_paused = true)]
    async fn a_stop_during_the_wait_for_the_tick_ends_the_pass_at_once() {
        let r = start(&[]).await;
        let _a = r.hub.register(&agent("a")).unwrap();
        settle().await;
        let _tasks = r.hub.begin_stop();
        settle().await;
        assert!(r.task.is_finished());
        assert!(r.calls.lock().unwrap().is_empty());
    }

    // C12
    #[tokio::test(start_paused = true)]
    async fn a_stop_during_the_read_ends_the_pass_and_closes_nobody() {
        let (hub, sessions) = (Hub::new(), Sessions::new());
        let mut a = hub.register(&agent("a")).unwrap();
        let task = tokio::spawn(run(hub.clone(), sessions, hub.stopping(), |_ids| {
            std::future::pending::<Result<Vec<String>, sqlx::Error>>()
        }));
        tokio::time::advance(PERIOD).await;
        settle().await;
        assert!(!task.is_finished(), "the read is in flight");
        let _tasks = hub.begin_stop();
        settle().await;
        assert!(task.is_finished());
        assert_eq!(closed(&mut a), None);
    }
}

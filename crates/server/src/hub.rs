//! The hub of the control channel (design 5.2, 13.3, 14.1; ADR-466, ADR-468): which connection is
//! an agent's current one, and the tasks that run them.
//!
//! axum runs the callback of an upgraded socket on a task it spawns and detaches, so the
//! connection could not be stopped. The route's callback only hands the session to
//! [`Hub::adopt`], and the hub spawns it into a [`JoinSet`] it owns: a stop can close every
//! session, wait for them within the grace period and abort the rest (design 13.3).
//!
//! The lock is a `std` mutex and is never held across an await.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use shared_proto::{AgentId, CLOSE_CODE_REPLACED};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;

/// The generation of one connection of an agent: a later connection of the same agent has a
/// larger value, so a connection that ends late cannot remove its successor's entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectionId(u64);

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The hub: cheap to clone, one per server.
#[derive(Clone, Default)]
pub struct Hub(Arc<Inner>);

impl fmt::Debug for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.0.lock();
        f.debug_struct("Hub")
            .field("connections", &state.current.len())
            .field("stopping", &state.tasks.is_none())
            .finish()
    }
}

struct Inner {
    state: Mutex<State>,
    /// True once the stop has begun; every session task watches it.
    stopping: watch::Sender<bool>,
}

impl Default for Inner {
    fn default() -> Inner {
        Inner {
            state: Mutex::new(State {
                next_id: 1,
                current: HashMap::new(),
                tasks: Some(JoinSet::new()),
            }),
            stopping: watch::Sender::new(false),
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct State {
    next_id: u64,
    /// The agent's current connection.
    current: HashMap<AgentId, Handle>,
    /// The session tasks; `None` once the stop has begun.
    tasks: Option<JoinSet<()>>,
}

struct Handle {
    id: ConnectionId,
    /// Tells the connection's task to end with a close code; the first instruction wins.
    close: Option<oneshot::Sender<u16>>,
}

/// What a session task gets when it becomes its agent's current connection.
pub(crate) struct Registration {
    pub(crate) id: ConnectionId,
    /// The close code the hub instructs the task to end with.
    pub(crate) close: oneshot::Receiver<u16>,
    pub(crate) stopping: watch::Receiver<bool>,
}

/// The hub is stopping and takes no more connections.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Stopping;

impl Hub {
    pub fn new() -> Hub {
        Hub::default()
    }

    /// Runs a session as a task the hub owns. Once the stop has begun the session is dropped
    /// unrun, which drops its socket without a frame: an upgrade that completes during a stop.
    pub(crate) fn adopt(&self, session: impl Future<Output = ()> + Send + 'static) {
        let mut state = self.0.lock();
        let Some(tasks) = state.tasks.as_mut() else {
            return;
        };
        // Reap the tasks that have ended, so the set does not grow with every connection.
        while let Some(finished) = tasks.try_join_next() {
            if let Err(error) = finished
                && error.is_panic()
            {
                tracing::warn!("a control channel session panicked");
            }
        }
        tasks.spawn(session);
    }

    /// Makes the caller `agent`'s current connection. The connection it displaces is told to
    /// end with the close code for replacement; both happen under one lock, so the agent never
    /// has two current connections or none (ADR-468).
    pub(crate) fn register(&self, agent: &AgentId) -> Result<Registration, Stopping> {
        let mut state = self.0.lock();
        if state.tasks.is_none() {
            return Err(Stopping);
        }
        let id = ConnectionId(state.next_id);
        state.next_id += 1;
        let (close_tx, close) = oneshot::channel();
        let displaced = state.current.insert(
            agent.clone(),
            Handle {
                id,
                close: Some(close_tx),
            },
        );
        if let Some(close) = displaced.and_then(|old| old.close) {
            let _ = close.send(CLOSE_CODE_REPLACED);
        }
        Ok(Registration {
            id,
            close,
            stopping: self.0.stopping.subscribe(),
        })
    }

    /// Removes the entry of connection `id` of `agent`, and says whether it was the agent's
    /// current one: a replaced connection finds the entry of its successor and removes nothing.
    pub(crate) fn leave(&self, agent: &AgentId, id: ConnectionId) -> bool {
        let mut state = self.0.lock();
        match state.current.get(agent) {
            Some(handle) if handle.id == id => {
                state.current.remove(agent);
                true
            }
            _ => false,
        }
    }

    /// Begins the stop: every session task is told, no further connection registers or is
    /// adopted, and the tasks are handed to the caller to wait for and abort. Called a second
    /// time it returns an empty set.
    pub(crate) fn begin_stop(&self) -> JoinSet<()> {
        let mut state = self.0.lock();
        let tasks = state.tasks.take();
        self.0.stopping.send_replace(true);
        tasks.unwrap_or_default()
    }

    #[cfg(test)]
    fn connected(&self) -> usize {
        self.0.lock().current.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str) -> AgentId {
        AgentId(name.to_owned())
    }

    #[test]
    fn a_newer_connection_replaces_the_older_with_one_close_instruction() {
        let hub = Hub::new();
        let a = agent("a");
        let first = hub.register(&a).unwrap();
        let mut first_close = first.close;
        assert!(first_close.try_recv().is_err());
        let second = hub.register(&a).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(first_close.try_recv(), Ok(CLOSE_CODE_REPLACED));
        assert_eq!(hub.connected(), 1);
    }

    #[test]
    fn a_replaced_connection_leaves_nothing_and_the_current_one_leaves_its_entry() {
        let hub = Hub::new();
        let a = agent("a");
        let first = hub.register(&a).unwrap();
        let second = hub.register(&a).unwrap();
        assert!(!hub.leave(&a, first.id));
        assert_eq!(hub.connected(), 1, "the successor's entry is kept");
        assert!(hub.leave(&a, second.id));
        assert_eq!(hub.connected(), 0);
        assert!(!hub.leave(&a, second.id), "a second leave removes nothing");
    }

    #[test]
    fn agents_do_not_replace_each_other() {
        let hub = Hub::new();
        let mut one = hub.register(&agent("a")).unwrap().close;
        let two = hub.register(&agent("b")).unwrap();
        assert!(one.try_recv().is_err());
        assert_eq!(hub.connected(), 2);
        assert!(hub.leave(&agent("b"), two.id));
    }

    #[tokio::test]
    async fn after_the_stop_began_nothing_registers_and_a_late_session_is_dropped_unrun() {
        let hub = Hub::new();
        let a = agent("a");
        let first = hub.register(&a).unwrap();
        let mut stopping = first.stopping;
        assert!(!*stopping.borrow());
        let tasks = hub.begin_stop();
        assert!(*stopping.borrow_and_update());
        assert_eq!(hub.register(&a).err(), Some(Stopping));

        let (ran, was_run) = oneshot::channel::<()>();
        hub.adopt(async move {
            let _ = ran.send(());
        });
        assert!(
            was_run.await.is_err(),
            "the session was dropped, so its sender is gone"
        );
        assert!(tasks.is_empty());
        assert!(hub.begin_stop().is_empty());
    }

    #[tokio::test]
    async fn an_adopted_session_runs_in_the_set_the_stop_returns() {
        let hub = Hub::new();
        let (release, held) = oneshot::channel::<()>();
        hub.adopt(async move {
            let _ = held.await;
        });
        let mut tasks = hub.begin_stop();
        assert_eq!(tasks.len(), 1);
        let _ = release.send(());
        assert!(tasks.join_next().await.unwrap().is_ok());
    }
}

//! The hub of the control channel (design 5.2, 13.3, 14.1; ADR-466, ADR-468): which connection is
//! an agent's current one, and the tasks that run them.
//!
//! axum runs the callback of an upgraded socket on a task it spawns and detaches, so the
//! connection could not be stopped. The route's callback only hands the session to
//! [`Hub::adopt`], and the hub spawns it into a [`JoinSet`] it owns: a stop can close every
//! session, wait for them within the grace period and abort the rest (design 13.3).
//!
//! The hub also holds the two queues of the channel (ADR-468 item 12): the bounded outbound queue
//! of each connection, fed by [`Hub::send_job_command`] and drained by the connection's task, and
//! the one bounded inbox that [`Hub::inbox`] hands out once, fed by every task. The hub keeps no
//! delivery table: a delivery is settled by the agent's answer, which the inbox's consumer matches
//! by connection and command ID.
//!
//! The lock is a `std` mutex and is never held across an await.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use shared_proto::{AgentId, AgentMessage, CLOSE_CODE_REPLACED, CLOSE_CODES, ServerMessage};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;

/// How many frames wait in one connection's outbound queue.
pub const OUTBOUND_CAPACITY: usize = 16;
/// How many messages wait in the inbox.
pub const INBOX_CAPACITY: usize = 256;

/// The generation of one connection of an agent: a later connection of the same agent has a
/// larger value, so a connection that ends late cannot remove its successor's entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

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
    /// The source of `commandId` numbers, for the whole hub.
    next_command: AtomicU64,
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
                inbox: None,
            }),
            next_command: AtomicU64::new(1),
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
    /// The inbox's sender, once [`Hub::inbox`] has handed out the receiver; until then a message
    /// is dropped, not buffered.
    inbox: Option<mpsc::Sender<Inbound>>,
}

struct Handle {
    id: ConnectionId,
    /// Tells the connection's task to end with a close code; the first instruction wins. `None`
    /// once it has been given: the connection is closing.
    close: Option<oneshot::Sender<u16>>,
    /// The frames waiting to be written to the connection.
    outbound: mpsc::Sender<String>,
}

/// What a session task gets when it becomes its agent's current connection.
pub(crate) struct Registration {
    pub(crate) id: ConnectionId,
    /// The close code the hub instructs the task to end with.
    pub(crate) close: oneshot::Receiver<u16>,
    pub(crate) stopping: watch::Receiver<bool>,
    /// The frames [`Hub::send_job_command`] queued, in call order.
    pub(crate) outbound: mpsc::Receiver<String>,
}

/// A job command queued on an agent's current connection. It means queued, not written: only the
/// agent's answer settles it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The connection the command was queued on.
    pub connection: ConnectionId,
    /// The `commandId` the frame carries: `c-` and a number that is unique in this hub.
    pub command_id: String,
}

/// Why [`Hub::send_job_command`] queued nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// The agent has no current connection, or it is closing.
    NotConnected,
    /// [`OUTBOUND_CAPACITY`] frames already wait on the connection.
    Busy,
    /// The frame is over the control message limit (128 KiB); the connection is untouched.
    TooLarge,
}

/// A message an agent sent that is not a heartbeat, as the inbox gives it.
#[derive(Debug, Clone)]
pub struct Inbound {
    pub agent: AgentId,
    /// The connection that read it.
    pub connection: ConnectionId,
    pub message: AgentMessage,
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
        let (outbound_tx, outbound) = mpsc::channel(OUTBOUND_CAPACITY);
        let displaced = state.current.insert(
            agent.clone(),
            Handle {
                id,
                close: Some(close_tx),
                outbound: outbound_tx,
            },
        );
        if let Some(close) = displaced.and_then(|old| old.close) {
            let _ = close.send(CLOSE_CODE_REPLACED);
        }
        Ok(Registration {
            id,
            close,
            stopping: self.0.stopping.subscribe(),
            outbound,
        })
    }

    /// Tells `agent`'s current connection to end with `code`, and says whether it was told: the
    /// instruction is given once, and `false` means there is no current connection or it has
    /// already been told. The entry stays until the connection's task leaves (ADR-468). Only the
    /// server's own close codes are sent; any other code (such as 1005, 1006 or 1015, which are
    /// never put on the wire) is refused with `false` and tells nobody.
    pub fn close_agent(&self, agent: &AgentId, code: u16) -> bool {
        if !CLOSE_CODES.contains(&code) {
            return false;
        }
        let mut state = self.0.lock();
        state
            .current
            .get_mut(agent)
            .and_then(|handle| handle.close.take())
            .is_some_and(|close| close.send(code).is_ok())
    }

    /// Queues a signed job on `agent`'s current connection. It does not wait and writes nothing
    /// itself: the connection's task writes the frame, after its `hello`, in the order of the
    /// calls. A call that fails still used its `commandId` number.
    pub fn send_job_command(
        &self,
        agent: &AgentId,
        signed_job: String,
    ) -> Result<Delivery, SendError> {
        let number = self.0.next_command.fetch_add(1, Ordering::Relaxed);
        let command_id = format!("c-{number}");
        // Encoded outside the lock; an oversized frame is refused before the connection is looked at.
        let frame = ServerMessage::JobCommand {
            command_id: command_id.clone(),
            signed_job: Some(signed_job),
        }
        .encode()
        // Serializing two strings fails only by the size limit; any other error is refused alike.
        .map_err(|_| SendError::TooLarge)?;
        let state = self.0.lock();
        if state.tasks.is_none() {
            return Err(SendError::NotConnected);
        }
        let handle = state.current.get(agent).ok_or(SendError::NotConnected)?;
        if handle.close.is_none() {
            return Err(SendError::NotConnected);
        }
        match handle.outbound.try_send(frame) {
            Ok(()) => Ok(Delivery {
                connection: handle.id,
                command_id,
            }),
            Err(TrySendError::Full(_)) => Err(SendError::Busy),
            Err(TrySendError::Closed(_)) => Err(SendError::NotConnected),
        }
    }

    /// The receiver of the inbox, to the first caller only: every message of an agent except a
    /// heartbeat arrives on it, in the order it was read. Until it is taken, and once it is
    /// dropped, such messages are dropped.
    pub fn inbox(&self) -> Option<mpsc::Receiver<Inbound>> {
        let mut state = self.0.lock();
        if state.inbox.is_some() {
            return None;
        }
        let (sender, receiver) = mpsc::channel(INBOX_CAPACITY);
        state.inbox = Some(sender);
        Some(receiver)
    }

    /// The inbox's sender while someone holds its receiver.
    pub(crate) fn inbox_sender(&self) -> Option<mpsc::Sender<Inbound>> {
        let state = self.0.lock();
        state.inbox.clone().filter(|sender| !sender.is_closed())
    }

    /// The agents that have a current connection, with its number.
    pub fn connected(&self) -> Vec<(AgentId, ConnectionId)> {
        let state = self.0.lock();
        state
            .current
            .iter()
            .map(|(agent, handle)| (agent.clone(), handle.id))
            .collect()
    }

    /// The stop flag, which turns true when the stop begins.
    pub(crate) fn stopping(&self) -> watch::Receiver<bool> {
        self.0.stopping.subscribe()
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
        assert_eq!(hub.connected().len(), 1);
    }

    #[test]
    fn a_replaced_connection_leaves_nothing_and_the_current_one_leaves_its_entry() {
        let hub = Hub::new();
        let a = agent("a");
        let first = hub.register(&a).unwrap();
        let second = hub.register(&a).unwrap();
        assert!(!hub.leave(&a, first.id));
        assert_eq!(hub.connected().len(), 1, "the successor's entry is kept");
        assert!(hub.leave(&a, second.id));
        assert_eq!(hub.connected().len(), 0);
        assert!(!hub.leave(&a, second.id), "a second leave removes nothing");
    }

    #[test]
    fn agents_do_not_replace_each_other() {
        let hub = Hub::new();
        let mut one = hub.register(&agent("a")).unwrap().close;
        let two = hub.register(&agent("b")).unwrap();
        assert!(one.try_recv().is_err());
        assert_eq!(hub.connected().len(), 2);
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

    // ----- the queues (ADR-468 item 12)

    use shared_proto::MAX_CONTROL_MESSAGE_LEN;

    /// The text of the frame for `signed_job` with the command ID `c-1`.
    fn frame_len(signed_job: &str) -> usize {
        ServerMessage::JobCommand {
            command_id: "c-1".to_owned(),
            signed_job: Some(signed_job.to_owned()),
        }
        .encode()
        .unwrap()
        .len()
    }

    // D1
    #[test]
    fn a_frame_of_exactly_128_kib_is_queued_and_one_byte_more_is_too_large_before_the_connection() {
        let overhead = frame_len("");
        let fits = "x".repeat(MAX_CONTROL_MESSAGE_LEN - overhead);
        assert_eq!(frame_len(&fits), MAX_CONTROL_MESSAGE_LEN);
        let hub = Hub::new();
        let a = agent("a");
        // Nobody is connected: the size is judged first. The first call has the ID `c-1`.
        assert_eq!(
            hub.send_job_command(&a, format!("{fits}x")),
            Err(SendError::TooLarge)
        );
        // The ID `c-2` has the same width, so the boundary holds for it.
        assert_eq!(
            hub.send_job_command(&a, fits.clone()),
            Err(SendError::NotConnected)
        );
        let mut reg = hub.register(&a).unwrap();
        assert_eq!(
            hub.send_job_command(&a, format!("{fits}x")),
            Err(SendError::TooLarge)
        );
        assert!(
            reg.outbound.try_recv().is_err(),
            "the connection is untouched"
        );
        assert!(hub.send_job_command(&a, fits).is_ok());
        assert_eq!(
            reg.outbound.try_recv().unwrap().len(),
            MAX_CONTROL_MESSAGE_LEN
        );
    }

    // D2, D3
    #[test]
    fn nothing_is_queued_for_an_absent_or_closing_connection() {
        let hub = Hub::new();
        let a = agent("a");
        assert_eq!(
            hub.send_job_command(&a, "j".to_owned()),
            Err(SendError::NotConnected)
        );
        // The close instruction has been taken.
        let reg = hub.register(&a).unwrap();
        assert!(hub.close_agent(&a, 4002));
        assert_eq!(
            hub.send_job_command(&a, "j".to_owned()),
            Err(SendError::NotConnected)
        );
        drop(reg);
        // The task's receiver is gone.
        let reg = hub.register(&a).unwrap();
        let Registration { outbound, .. } = reg;
        drop(outbound);
        assert_eq!(
            hub.send_job_command(&a, "j".to_owned()),
            Err(SendError::NotConnected)
        );
        // The stop has begun.
        let _reg = hub.register(&agent("b")).unwrap();
        let _tasks = hub.begin_stop();
        assert_eq!(
            hub.send_job_command(&agent("b"), "j".to_owned()),
            Err(SendError::NotConnected)
        );
    }

    // D4
    #[test]
    fn the_seventeenth_waiting_frame_is_busy_until_one_is_taken() {
        let hub = Hub::new();
        let a = agent("a");
        let mut reg = hub.register(&a).unwrap();
        for _ in 0..OUTBOUND_CAPACITY {
            hub.send_job_command(&a, "j".to_owned()).unwrap();
        }
        assert_eq!(
            hub.send_job_command(&a, "j".to_owned()),
            Err(SendError::Busy)
        );
        reg.outbound.try_recv().unwrap();
        assert!(hub.send_job_command(&a, "j".to_owned()).is_ok());
    }

    // D5, D6, D12
    #[test]
    fn deliveries_name_the_connection_and_carry_unique_ids_in_call_order() {
        let hub = Hub::new();
        let (a, b) = (agent("a"), agent("b"));
        let mut reg_a = hub.register(&a).unwrap();
        let mut reg_b = hub.register(&b).unwrap();
        let first = hub.send_job_command(&a, "one".to_owned()).unwrap();
        assert_eq!(first.command_id, "c-1");
        assert_eq!(first.connection, reg_a.id);
        // A failed call consumes its number.
        assert_eq!(
            hub.send_job_command(&agent("nobody"), "x".to_owned()),
            Err(SendError::NotConnected)
        );
        let second = hub.send_job_command(&b, "two".to_owned()).unwrap();
        assert_eq!(second.command_id, "c-3", "the number is hub-wide");
        assert_eq!(second.connection, reg_b.id);
        let third = hub.send_job_command(&a, "three".to_owned()).unwrap();
        let ids: Vec<String> = [&first, &second, &third]
            .iter()
            .map(|d| d.command_id.clone())
            .collect();
        assert_eq!(ids, ["c-1", "c-3", "c-4"]);
        // The wire order of one agent is the order of the calls.
        let wire = |text: String| match ServerMessage::decode(&text).unwrap() {
            ServerMessage::JobCommand {
                command_id,
                signed_job,
            } => (command_id, signed_job.unwrap()),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            wire(reg_a.outbound.try_recv().unwrap()),
            ("c-1".to_owned(), "one".to_owned())
        );
        assert_eq!(
            wire(reg_a.outbound.try_recv().unwrap()),
            ("c-4".to_owned(), "three".to_owned())
        );
        assert_eq!(
            wire(reg_b.outbound.try_recv().unwrap()),
            ("c-3".to_owned(), "two".to_owned())
        );
    }

    // D7
    #[test]
    fn a_frame_queued_before_a_replacement_stays_with_the_replaced_connection() {
        let hub = Hub::new();
        let a = agent("a");
        let mut old = hub.register(&a).unwrap();
        hub.send_job_command(&a, "j".to_owned()).unwrap();
        let mut new = hub.register(&a).unwrap();
        assert!(
            new.outbound.try_recv().is_err(),
            "nothing moves to the successor"
        );
        let delivery = hub.send_job_command(&a, "k".to_owned()).unwrap();
        assert_eq!(delivery.connection, new.id);
        assert!(new.outbound.try_recv().is_ok());
        // The old task never writes what it still holds (its close instruction comes first), and
        // its queue has no sender left once the entry is replaced.
        assert!(old.outbound.try_recv().is_ok());
        assert!(matches!(
            old.outbound.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    // C10 (the hub's half)
    #[test]
    fn close_agent_gives_its_instruction_once_and_keeps_the_entry() {
        let hub = Hub::new();
        let a = agent("a");
        assert!(!hub.close_agent(&a, 4002), "no entry");
        let mut reg = hub.register(&a).unwrap();
        assert!(hub.close_agent(&a, 4002));
        assert_eq!(reg.close.try_recv(), Ok(4002));
        assert!(!hub.close_agent(&a, 4002), "already instructed");
        assert_eq!(hub.connected(), vec![(a.clone(), reg.id)]);
        // A later connection is not told by the earlier instruction.
        let mut next = hub.register(&a).unwrap();
        assert!(next.close.try_recv().is_err());
    }

    #[test]
    fn close_agent_refuses_a_code_that_is_not_the_servers_own() {
        let hub = Hub::new();
        let a = agent("a");
        let mut reg = hub.register(&a).unwrap();
        for code in [1000, 1005, 1006, 1015, 4000, 4004, 0, u16::MAX] {
            assert!(!hub.close_agent(&a, code), "code {code}");
        }
        assert!(reg.close.try_recv().is_err(), "nobody was told");
        for code in [1001, 1002, 1003, 1009, 4001, 4002, 4003] {
            let mut reg = hub.register(&a).unwrap();
            assert!(hub.close_agent(&a, code), "code {code}");
            assert_eq!(reg.close.try_recv(), Ok(code));
        }
    }

    // E1
    #[test]
    fn the_inbox_goes_to_the_first_caller_only() {
        let hub = Hub::new();
        assert!(
            hub.inbox_sender().is_none(),
            "untaken: dropped, not buffered"
        );
        let receiver = hub.inbox().expect("the first call");
        assert!(hub.inbox().is_none());
        assert!(hub.inbox_sender().is_some());
        drop(receiver);
        assert!(hub.inbox().is_none(), "never handed out again");
        assert!(
            hub.inbox_sender().is_none(),
            "a dropped receiver takes nothing"
        );
    }

    // B6, B7: whether a connection that ends was the agent's current one is decided by `leave`,
    // at the moment it leaves.
    #[test]
    fn leave_says_current_for_a_connection_that_ended_before_its_successor_registered() {
        let hub = Hub::new();
        let a = agent("a");
        let first = hub.register(&a).unwrap();
        assert!(hub.leave(&a, first.id), "B6: it ended on its own");
        let second = hub.register(&a).unwrap();
        assert!(hub.leave(&a, second.id));

        // B7: the pass told it to close, and a successor registered before it left.
        let closed = hub.register(&a).unwrap();
        assert!(hub.close_agent(&a, 4002));
        let successor = hub.register(&a).unwrap();
        assert!(!hub.leave(&a, closed.id));
        assert!(hub.leave(&a, successor.id));
    }
}

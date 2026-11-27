//! The outbox as the control channel uses it (design 5.3, 5.4; ADR-478): [`SharedOutbox`], the
//! open outbox that the channel and the producers of events share, and [`Sender`], one
//! connection's cursor over it.
//!
//! An event is pending in the outbox once its append has returned; the connection sends the
//! pending events in order after `hello`, at most [`SEND_WINDOW`] past the number the server has
//! acknowledged, and sends a later append at once. What was sent on a connection that ended before
//! its acknowledgement is sent again by the next connection from the oldest pending event, with
//! the same text. An acknowledgement is cumulative; one above the highest number assigned is a
//! protocol violation ([`Acked::OutOfRange`]).
//!
//! The outbox's file operations block, so they run on blocking tasks; the connection's loop only
//! ever awaits them.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use shared_proto::{AgentEventMessage, AgentMessage, ControlError, JobId, JobState};
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

use crate::outbox::{Kind, Outbox, OutboxError};

/// How many events may be on the wire past the highest number the server has acknowledged.
pub(super) const SEND_WINDOW: u64 = 64;
/// How long acknowledgements are taken in memory only after one whose rewrite of the file failed:
/// the outbox tries the owed rewrite again at every acknowledgement and has no delay of its own.
pub(super) const OWED_PAUSE: Duration = Duration::from_secs(10);

/// The open outbox, shared by the control channel and whoever produces events. Cloning is cheap.
#[derive(Clone)]
pub struct SharedOutbox {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for SharedOutbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedOutbox").finish_non_exhaustive()
    }
}

struct Inner {
    outbox: Mutex<Outbox>,
    /// An event was appended: the connected loop looks for something to send.
    added: Notify,
}

impl SharedOutbox {
    pub fn new(outbox: Outbox) -> SharedOutbox {
        SharedOutbox {
            inner: Arc::new(Inner {
                outbox: Mutex::new(outbox),
                added: Notify::new(),
            }),
        }
    }

    /// Appends an event on a blocking task (see [`Outbox::append`]) and wakes the connection that
    /// is waiting for events. The append goes on if this future is dropped.
    pub async fn append(
        &self,
        kind: Kind,
        text: impl FnOnce(u64) -> String + Send + 'static,
    ) -> Result<u64, OutboxError> {
        let inner = self.inner.clone();
        let appended = tokio::task::spawn_blocking(move || {
            let result = inner.outbox.blocking_lock().append(kind, text);
            if result.is_ok() {
                // One permit is kept when nobody waits, so the next wait ends at once.
                inner.added.notify_one();
            }
            result
        })
        .await;
        appended.unwrap_or_else(|_| Err(task_failed()))
    }

    /// Appends the `jobState` event of `job_id` in `state` (see [`SharedOutbox::append`]): the
    /// text is checked to encode at the longest number the counter gives before anything is
    /// appended, so the text appended encodes as well. No device time is given: the property is
    /// optional and the server stamps its own receive time (16.3).
    pub async fn append_job_state(
        &self,
        kind: Kind,
        job_id: String,
        state: JobState,
    ) -> Result<u64, JobStateError> {
        job_state_text(&job_id, u64::MAX - 1, &state).map_err(JobStateError::Text)?;
        Ok(self
            .append(kind, move |seq| {
                job_state_text(&job_id, seq, &state).unwrap_or_default()
            })
            .await?)
    }

    /// Reopens the outbox on a blocking task when it is poisoned (see [`Outbox::reopen`]);
    /// `Ok(true)` when it did, `Ok(false)` when it was not poisoned. The session calls it before
    /// each connection attempt, so the reconnect delay paces the retries.
    pub(super) async fn reopen_if_poisoned(&self) -> Result<bool, OutboxError> {
        let inner = self.inner.clone();
        let reopened = tokio::task::spawn_blocking(move || {
            let mut outbox = inner.outbox.blocking_lock();
            if !outbox.is_poisoned() {
                return Ok(false);
            }
            outbox.reopen().map(|()| true)
        })
        .await;
        reopened.unwrap_or_else(|_| Err(task_failed()))
    }

    /// The numbers of the events still pending, for a test that checks nothing was acknowledged.
    #[cfg(test)]
    pub(super) async fn pending_for_test(&self) -> Vec<u64> {
        self.inner
            .outbox
            .lock()
            .await
            .pending()
            .map(|(seq, _)| seq)
            .collect()
    }

    /// The texts of the events still pending, in order, for a test that reads what the server
    /// would be sent.
    #[cfg(test)]
    pub(crate) async fn texts_for_test(&self) -> Vec<String> {
        self.inner
            .outbox
            .lock()
            .await
            .pending()
            .map(|(_, text)| text.to_owned())
            .collect()
    }

    #[cfg(test)]
    pub(super) async fn is_poisoned_for_test(&self) -> bool {
        self.inner.outbox.lock().await.is_poisoned()
    }

    /// The next event to send after number `sent`, once there is one within the window. Not
    /// cancel-unsafe: it only reads.
    async fn next_event(&self, sent: u64) -> (u64, String) {
        loop {
            let notified = self.inner.added.notified();
            {
                let outbox = self.inner.outbox.lock().await;
                let limit = outbox.acked().saturating_add(SEND_WINDOW);
                let next = outbox.pending().find(|(seq, _)| *seq > sent);
                if let Some((seq, text)) = next.filter(|(seq, _)| *seq <= limit) {
                    return (seq, text.to_owned());
                }
            }
            notified.await;
        }
    }
}

/// The text of the `jobState` event of `job_id` in `state` with number `seq`.
fn job_state_text(job_id: &str, seq: u64, state: &JobState) -> Result<String, ControlError> {
    AgentMessage::AgentEvent(AgentEventMessage {
        seq: Some(seq),
        occurred_at_agent: None,
        event: state.to_event_kind(JobId(job_id.to_owned())),
    })
    .encode()
}

/// Why a job state's event was not appended.
#[derive(Debug, thiserror::Error)]
pub enum JobStateError {
    /// The event's text cannot be built: nothing was appended.
    #[error("the job state's event cannot be built")]
    Text(#[source] ControlError),
    #[error(transparent)]
    Outbox(#[from] OutboxError),
}

fn task_failed() -> OutboxError {
    OutboxError::Io(io::Error::other("the outbox task failed"))
}

/// What an acknowledgement did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Acked {
    Taken,
    /// Above the highest number assigned: nothing changed.
    OutOfRange,
    /// Taken, but the rewrite it started poisoned the outbox: the connection closes so that the
    /// next one reopens it.
    Poisoned,
}

/// One connection's view of the outbox.
pub(super) struct Sender {
    outbox: SharedOutbox,
    /// The highest number written on this connection.
    sent: u64,
    /// While set and in the future, acknowledgements are taken in memory and the owed rewrite
    /// is not tried.
    paused_until: Option<Instant>,
}

impl Sender {
    pub(super) fn new(outbox: &SharedOutbox) -> Sender {
        Sender {
            outbox: outbox.clone(),
            sent: 0,
            paused_until: None,
        }
    }

    /// Waits for the next event to write: the oldest pending one this connection has not sent,
    /// within the window. Call [`Sender::wrote`] once it is written.
    pub(super) async fn next(&self) -> (u64, String) {
        self.outbox.next_event(self.sent).await
    }

    /// The event `seq` was written.
    pub(super) fn wrote(&mut self, seq: u64) {
        self.sent = seq;
    }

    /// Applies the server's cumulative acknowledgement on a blocking task: the events leave the
    /// pending ones at once, and the file is rewritten when the outbox says so. After a rewrite
    /// failed, the acknowledgements of the next [`OWED_PAUSE`] are taken in memory only, so the
    /// owed rewrite is tried once per pause and not once per acknowledgement.
    pub(super) async fn ack(&mut self, n: u64) -> Acked {
        let paused = self
            .paused_until
            .is_some_and(|until| Instant::now() < until);
        let inner = self.outbox.inner.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut outbox = inner.outbox.blocking_lock();
            if paused {
                return outbox.ack_without_rewrite(n).ok().map(|()| None);
            }
            let was_poisoned = outbox.is_poisoned();
            outbox
                .ack(n)
                .ok()
                .map(|()| Some((outbox.rewrite_owed(), !was_poisoned && outbox.is_poisoned())))
        })
        .await;
        match result {
            Ok(Some(None)) => Acked::Taken,
            Ok(Some(Some((_, true)))) => Acked::Poisoned,
            Ok(Some(Some((owed, false)))) => {
                self.paused_until = owed.then(|| Instant::now() + OWED_PAUSE);
                Acked::Taken
            }
            // `ack` fails only with `OutOfRange`; a task that failed is treated alike, since
            // closing the connection is the safe side.
            Ok(None) | Err(_) => Acked::OutOfRange,
        }
    }
}

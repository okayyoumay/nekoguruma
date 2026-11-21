//! The agent's data channel client (design 5.2, 5.4, 13.3, ADR-493, ADR-495): the library the uploader's
//! loop is built from.
//!
//! - [`http`]: the HTTP/1 client for the four routes and the part PUT: one request per
//!   connection, no redirect, a head and a JSON answer within bounds, a body streamed in chunks,
//!   and the no-progress clock of design 5.4
//! - [`part_url`]: the host rule a part URL must meet before the agent sends a byte to it
//!   (ADR-493 item 13)
//! - [`routes`]: the four routes' requests and strict answers, and how each answer is classified
//!   for the loop: deliver, retry, pass over, or exchange a new token
//!
//! - [`source`]: the check that an entry's bytes are what it recorded, and the reader of a part
//! - [`transfer`]: one attempt on one entry (ADR-495 item 5)
//!
//! On top of them is the loop ([`start`], [`Handle`]): it takes the entries of the upload ledger
//! ([`SharedLedger`], design 5.2, ADR-493 item 10) one at a time, results before logs, and is
//! the only writer of the back-off, the session token and the ledger's removals. It exchanges a
//! token only while an entry is pending, repeats a request once after a 401, passes over an entry
//! the server refuses (it is tried again on every pass), sets one aside when its completion fails
//! twice in a row, and abandons only one whose source it proved broken (ADR-495 items 5 to 10).
//! A refusal, an abandon and a set-aside are warned once per entry and class and counted in the
//! [`Status`] (item 11); the routes, the client and the source never log. A token, a URL and a
//! body are never put in an error or a log line.

pub mod http;
pub mod part_url;
pub mod routes;
mod source;
mod transfer;

/// The in-memory server the unit tests of this module and of the exchange's second caller use.
#[cfg(test)]
pub(crate) mod fake;

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use shared_crypto::proof::Audience;
use shared_crypto::{RandomError, Signer};
use tokio::sync::{Mutex, Notify, watch};

use crate::control::ControlError;
use crate::control::backoff::Backoff;
use crate::control::dial::{LoopbackPorts, PortDial};
use crate::control::exchange::{self, Token};
use crate::identity::AgentKey;
use crate::journal::JobKey;
use crate::registration::origin::ServerOrigin;
use crate::registration::record::{Record, RecordError};
use crate::upload_ledger::{Digest, Entry, LedgerError, UploadLedger};
use http::Client;
use routes::Routes;

/// The open upload ledger, shared by the loop and whoever enqueues (design 5.2, ADR-493 item
/// 10). Cloning is cheap. Every call that touches the file runs on a blocking task, and an enqueue
/// wakes the loop, also a failed one, so that a loop that is idle on a poisoned ledger reopens
/// it.
#[derive(Clone)]
pub struct SharedLedger {
    inner: Arc<LedgerInner>,
}

struct LedgerInner {
    ledger: Mutex<UploadLedger>,
    /// An enqueue was tried: the idle loop looks for work.
    added: Notify,
}

impl SharedLedger {
    pub fn new(ledger: UploadLedger) -> SharedLedger {
        SharedLedger {
            inner: Arc::new(LedgerInner {
                ledger: Mutex::new(ledger),
                added: Notify::new(),
            }),
        }
    }

    /// [`UploadLedger::enqueue_result`] on a blocking task. The write goes on if this future is
    /// dropped.
    pub async fn enqueue_result(&self, key: JobKey, bytes: Vec<u8>) -> Result<u64, LedgerError> {
        self.blocking(move |ledger| ledger.enqueue_result(&key, &bytes), true)
            .await
    }

    /// [`UploadLedger::enqueue_comm_log`] on a blocking task.
    pub async fn enqueue_comm_log(
        &self,
        key: JobKey,
        path: PathBuf,
        digest: Digest,
    ) -> Result<u64, LedgerError> {
        self.blocking(
            move |ledger| ledger.enqueue_comm_log(&key, &path, digest),
            true,
        )
        .await
    }

    async fn blocking<T: Send + 'static>(
        &self,
        call: impl FnOnce(&mut UploadLedger) -> Result<T, LedgerError> + Send + 'static,
        wake: bool,
    ) -> Result<T, LedgerError> {
        let inner = self.inner.clone();
        let done = tokio::task::spawn_blocking(move || {
            let result = call(&mut inner.ledger.blocking_lock());
            if wake {
                // One permit is kept when nobody waits, so the next wait ends at once.
                inner.added.notify_one();
            }
            result
        })
        .await;
        done.unwrap_or_else(|_| {
            Err(LedgerError::Io(std::io::Error::other(
                "the ledger task failed",
            )))
        })
    }

    /// The entries in upload order, with where their bytes are. Served from memory.
    async fn pending(&self) -> Vec<Pending> {
        let ledger = self.inner.ledger.lock().await;
        ledger
            .pending()
            .map(|entry| Pending {
                path: ledger.object_path(entry),
                entry: entry.clone(),
            })
            .collect()
    }

    /// Reopens the ledger when it is poisoned: `Ok(true)` when it did.
    async fn reopen_if_poisoned(&self) -> Result<bool, LedgerError> {
        self.blocking(
            |ledger| {
                if !ledger.is_poisoned() {
                    return Ok(false);
                }
                ledger.reopen().map(|()| true)
            },
            false,
        )
        .await
    }

    async fn finished(&self, seq: u64) -> Result<(), LedgerError> {
        self.blocking(move |ledger| ledger.finished(seq), false)
            .await
    }

    async fn abandon(&self, seq: u64) -> Result<(), LedgerError> {
        self.blocking(move |ledger| ledger.abandon(seq), false)
            .await
    }

    async fn added(&self) {
        self.inner.added.notified().await;
    }
}

/// An entry and the file its bytes are in.
struct Pending {
    entry: Entry,
    path: PathBuf,
}

/// What [`Handle::status`] shows: counts over the whole run (ADR-495 item 11).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Entries the server has (and the ledger no longer holds).
    pub delivered: u64,
    /// Times an entry was passed over for the server's refusal or a ledger write.
    pub passed_over: u64,
    /// Entries removed because their source was proved broken.
    pub abandoned: u64,
    /// Entries set aside until a restart (a completion that failed twice in a row).
    pub set_aside: u64,
    /// Attempts that ended in a retry.
    pub retries: u64,
}

/// Why the task ended when no stop asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// A reopen found the ledger file corrupt, or the agent ID unusable for it.
    LedgerUnusable,
}

/// The running loop. Dropping it stops the loop, like [`Handle::stop`].
pub struct Handle {
    stop: watch::Sender<bool>,
    status: watch::Receiver<Status>,
    done: watch::Receiver<bool>,
    ended: Arc<OnceLock<Ended>>,
}

impl Handle {
    /// Asks the loop to stop: the request in flight is dropped, a ledger write that is running is
    /// awaited. A second call does nothing more.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    /// Resolves once the task has ended.
    pub async fn stopped(&self) {
        let mut done = self.done.clone();
        let _ = done.wait_for(|done| *done).await;
    }

    /// Why the task ended by itself, once it has; `None` for a stop and while it runs.
    pub fn ended(&self) -> Option<Ended> {
        self.ended.get().copied()
    }

    /// The counters.
    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle").finish_non_exhaustive()
    }
}

/// The stop signal as the loop sees it. A handle that is gone is a stop too.
#[derive(Clone)]
struct Stop(watch::Receiver<bool>);

impl Stop {
    async fn requested(&self) {
        let mut stop = self.0.clone();
        let _ = stop.wait_for(|stop| *stop).await;
    }

    fn is_set(&self) -> bool {
        *self.0.borrow() || self.0.has_changed().is_err()
    }
}

/// A stop came: the request in flight was dropped.
struct Stopped;

/// Runs `fut` until a stop.
async fn until_stop<T>(stop: &Stop, fut: impl Future<Output = T>) -> Result<T, Stopped> {
    tokio::select! {
        biased;
        () = stop.requested() => Err(Stopped),
        value = fut => Ok(value),
    }
}

/// What the loop is wired to; tests replace the client, the dial, the signer and the random
/// source.
struct Seams {
    client: Client,
    dial: Arc<dyn PortDial>,
    signer: Arc<dyn Signer + Send + Sync>,
    random: fn() -> Result<[u8; 32], RandomError>,
}

/// Starts the uploader's loop for the registered agent `record`, over `ledger`. It needs a
/// running tokio runtime and sends nothing until an entry is pending. The record's server must be
/// the loopback origin it was registered with, and `key` the key it names.
pub fn start(
    record: &Record,
    key: &AgentKey,
    ledger: SharedLedger,
) -> Result<Handle, ControlError> {
    let seams = Seams {
        client: Client::loopback(),
        dial: Arc::new(LoopbackPorts),
        signer: key.signer(),
        random: shared_crypto::random_32,
    };
    start_with(record, &key.public_key(), ledger, seams)
}

fn start_with(
    record: &Record,
    key: &shared_crypto::PublicKey,
    ledger: SharedLedger,
    seams: Seams,
) -> Result<Handle, ControlError> {
    let (stop_tx, stop_rx) = watch::channel(false);
    let (status_tx, status_rx) = watch::channel(Status::default());
    let (done_tx, done_rx) = watch::channel(false);
    let worker = Worker::new(record, key, ledger, seams, Stop(stop_rx), status_tx)?;
    let ended = Arc::new(OnceLock::new());
    let task_ended = ended.clone();
    tokio::spawn(async move {
        let _done = Done(done_tx);
        worker.run(task_ended).await;
    });
    Ok(Handle {
        stop: stop_tx,
        status: status_rx,
        done: done_rx,
        ended,
    })
}

/// Says the task has ended, however it ends.
struct Done(watch::Sender<bool>);

impl Drop for Done {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

/// The exchange's log lines in the uploader's words: one warning per class until a token is
/// issued, debug after that, and one per refusal until a token is issued again.
#[derive(Default)]
struct ExchangeLog {
    seen: HashSet<&'static str>,
    refused: bool,
}

impl exchange::Log for ExchangeLog {
    fn failure(&mut self, class: &'static str, status: Option<u16>) {
        if self.seen.insert(class) {
            tracing::warn!(class, status = ?status, "uploader exchange attempt failed");
        } else {
            tracing::debug!(class, status = ?status, "uploader exchange attempt failed");
        }
    }

    fn refusal(&mut self, class: &'static str, status: u16) {
        if self.refused {
            tracing::debug!(class, status, "uploader exchange refused again");
        } else {
            self.refused = true;
            tracing::warn!(class, status, "uploader exchange refused");
        }
    }

    fn accepted(&mut self) {
        self.seen.clear();
        self.refused = false;
    }

    fn stale_challenge(&mut self) {
        tracing::debug!("uploader challenge was not live; asking for a new one");
    }
}

/// How an entry leaves the ledger.
#[derive(Clone, Copy)]
enum Removal {
    Finished,
    Abandon,
}

/// The loop's state. It is the only writer of the back-off, the token and the ledger's removals.
struct Worker {
    routes: Routes,
    dial: Arc<dyn PortDial>,
    signer: Arc<dyn Signer + Send + Sync>,
    random: fn() -> Result<[u8; 32], RandomError>,
    agent_id: String,
    audience: Audience,
    host: String,
    port: u16,
    ledger: SharedLedger,
    stop: Stop,
    status: watch::Sender<Status>,
    /// Held only while an entry is pending; not tracked by age (a 401 drops it).
    token: Option<Token>,
    backoff: Backoff,
    /// The next request waits the back-off first.
    owe_wait: bool,
    /// A `Retry-After` the server named, which lengthens the next wait.
    retry_after: Option<u32>,
    /// The attempt stored a part or got `finished`.
    progress: bool,
    log: ExchangeLog,
    /// Entries whose source was verified this run.
    verified: HashSet<u64>,
    /// Entries passed over in the current pass.
    passed_over: HashSet<u64>,
    /// Entries set aside until a restart.
    set_aside: HashSet<u64>,
    /// Entries whose last completion answered `failed`.
    failed_once: HashSet<u64>,
    /// Entries whose last completion answered `open`, which a second one in a row passes over.
    open_completions: HashSet<u64>,
    /// One warning per entry and class.
    warned: HashSet<(u64, &'static str)>,
    reopen_warned: bool,
}

impl Worker {
    fn new(
        record: &Record,
        key: &shared_crypto::PublicKey,
        ledger: SharedLedger,
        seams: Seams,
        stop: Stop,
        status: watch::Sender<Status>,
    ) -> Result<Worker, ControlError> {
        if key.fingerprint_hex() != record.agent_key_fingerprint {
            return Err(ControlError::KeyMismatch);
        }
        let origin = ServerOrigin::parse(&record.server_origin)
            .map_err(|_| RecordError::Field("server_origin"))?;
        let audience = Audience::parse_canonical(&record.server_origin)
            .map_err(|_| RecordError::Field("server_origin"))?;
        Ok(Worker {
            routes: Routes::new(seams.client, origin.clone()),
            dial: seams.dial,
            signer: seams.signer,
            random: seams.random,
            agent_id: record.agent_id.clone(),
            audience,
            host: origin.host_header(),
            port: origin.port(),
            ledger,
            stop,
            status,
            token: None,
            backoff: Backoff::new(),
            owe_wait: false,
            retry_after: None,
            progress: false,
            log: ExchangeLog::default(),
            verified: HashSet::new(),
            passed_over: HashSet::new(),
            set_aside: HashSet::new(),
            failed_once: HashSet::new(),
            open_completions: HashSet::new(),
            warned: HashSet::new(),
            reopen_warned: false,
        })
    }

    async fn run(mut self, ended: Arc<OnceLock<Ended>>) {
        loop {
            if self.stop.is_set() {
                break;
            }
            // A poisoned ledger is opened again here; only a file that does not read back ends
            // the task, and another failure keeps uploading from memory.
            let reopened = self.ledger.reopen_if_poisoned().await;
            if self.stop.is_set() {
                // A stop that came during the reopen ends the task quietly, whatever it found.
                break;
            }
            let mut poisoned = false;
            match reopened {
                Ok(true) => {
                    self.reopen_warned = false;
                    tracing::info!("the upload ledger was opened again");
                }
                Ok(false) => {}
                Err(error @ (LedgerError::Corrupt(_) | LedgerError::BadAgentId)) => {
                    tracing::error!(%error, "the upload ledger cannot be opened again; the uploader ends");
                    let _ = ended.set(Ended::LedgerUnusable);
                    break;
                }
                Err(error) => {
                    if !self.reopen_warned {
                        self.reopen_warned = true;
                        tracing::warn!(%error, "the upload ledger could not be opened again; uploading from memory");
                    }
                    self.owe_wait = true;
                    poisoned = true;
                }
            }
            let pending = self.ledger.pending().await;
            self.forget_gone(&pending);
            let eligible = |w: &Worker| {
                pending.iter().find(|p| {
                    !w.passed_over.contains(&p.entry.seq) && !w.set_aside.contains(&p.entry.seq)
                })
            };
            let mut job = eligible(&self);
            if job.is_none() && !self.passed_over.is_empty() {
                // Every pending entry was passed over: start again from the first; the owed wait
                // is taken at the start of the attempt.
                self.passed_over.clear();
                job = eligible(&self);
            }
            let Some(job) = job else {
                // Nothing to do: no token is held, and no request is made, until an enqueue. A
                // ledger that could not be opened again may hold entries memory does not (an
                // append that landed before its sync failed), so it is tried again at the
                // back-off's pace too, not only when an enqueue wakes the loop.
                self.token = None;
                if poisoned {
                    let wait = self.backoff.wait(None);
                    tokio::select! {
                        biased;
                        () = self.stop.requested() => break,
                        () = self.ledger.added() => {}
                        () = tokio::time::sleep(wait) => self.backoff.after_wait(),
                    }
                    self.owe_wait = false;
                } else {
                    tokio::select! {
                        biased;
                        () = self.stop.requested() => break,
                        () = self.ledger.added() => {}
                    }
                }
                continue;
            };
            let outcome = transfer::attempt(&mut self, job).await;
            if !self.settle(job, outcome).await {
                break;
            }
        }
    }

    /// Drops what the loop remembers of entries that are no longer pending.
    fn forget_gone(&mut self, pending: &[Pending]) {
        let held: HashSet<u64> = pending.iter().map(|p| p.entry.seq).collect();
        self.verified.retain(|seq| held.contains(seq));
        self.passed_over.retain(|seq| held.contains(seq));
        self.set_aside.retain(|seq| held.contains(seq));
        self.failed_once.retain(|seq| held.contains(seq));
        self.open_completions.retain(|seq| held.contains(seq));
        self.warned.retain(|(seq, _)| held.contains(seq));
    }

    /// Acts on what an attempt came to. `false` ends the loop.
    async fn settle(&mut self, job: &Pending, outcome: transfer::Outcome) -> bool {
        use transfer::Outcome;
        let seq = job.entry.seq;
        match outcome {
            Outcome::Stopped => return false,
            Outcome::Delivered => {
                if self.remove(job, Removal::Finished).await {
                    self.backoff.reset();
                    self.count(|s| s.delivered += 1);
                }
            }
            Outcome::Abandon(class) => {
                if self.remove(job, Removal::Abandon).await {
                    self.warn(job, class, None);
                    self.count(|s| s.abandoned += 1);
                }
            }
            Outcome::Retry(class, status) => {
                if !self.stop.is_set() {
                    tracing::debug!(entry = seq, class, status = ?status, "an upload attempt will be retried");
                }
                self.count(|s| s.retries += 1);
            }
            Outcome::PassOver(class, status) => {
                self.warn(job, class, status);
                self.passed_over.insert(seq);
                self.count(|s| s.passed_over += 1);
            }
            Outcome::SetAside(class) => {
                self.warn(job, class, None);
                self.set_aside.insert(seq);
                self.count(|s| s.set_aside += 1);
            }
        }
        if !self.progress {
            self.owe_wait = true;
        }
        if !self.owe_wait {
            self.retry_after = None;
        }
        true
    }

    /// Takes the entry out of the ledger. When that fails and the entry is still pending, it is
    /// passed over and the next wait is owed: the next pass's begin answers `finished` again (or
    /// the source check abandons again) and the removal is retried at the back-off's pace with
    /// no byte sent; `false` then. An entry the
    /// failed write already removed from memory needs nothing more: the reopen at the loop's top
    /// settles the file.
    async fn remove(&mut self, job: &Pending, how: Removal) -> bool {
        let seq = job.entry.seq;
        let removed = match how {
            Removal::Finished => self.ledger.finished(seq).await,
            Removal::Abandon => self.ledger.abandon(seq).await,
        };
        let Err(error) = removed else {
            return true;
        };
        let still = self
            .ledger
            .pending()
            .await
            .iter()
            .any(|p| p.entry.seq == seq);
        if !still {
            return true;
        }
        if self.stop.is_set() {
            return false;
        }
        self.warn_with(job, "ledger-write", None, Some(&error));
        self.passed_over.insert(seq);
        self.count(|s| s.passed_over += 1);
        self.owe_wait = true;
        false
    }

    fn count(&self, change: impl FnOnce(&mut Status)) {
        self.status.send_modify(change);
    }

    /// One warning per entry and class, debug after that: the entry's number, kind, the class and
    /// the status, never a URL, token or digest. Returns whether it was the warning.
    fn warn(&mut self, job: &Pending, class: &'static str, status: Option<u16>) -> bool {
        self.warn_with(job, class, status, None)
    }

    fn warn_with(
        &mut self,
        job: &Pending,
        class: &'static str,
        status: Option<u16>,
        error: Option<&LedgerError>,
    ) -> bool {
        if self.stop.is_set() {
            // After a stop nothing more is logged.
            return false;
        }
        let (entry, kind) = (job.entry.seq, job.entry.kind);
        let first = self.warned.insert((entry, class));
        match (first, error) {
            (true, None) => {
                tracing::warn!(entry, kind = ?kind, class, status = ?status, "an upload was not delivered")
            }
            (true, Some(error)) => {
                tracing::warn!(entry, kind = ?kind, class, %error, "an upload was not delivered")
            }
            (false, _) => {
                tracing::debug!(entry, kind = ?kind, class, status = ?status, "an upload was not delivered")
            }
        }
        first
    }
}

#[cfg(test)]
mod tests;

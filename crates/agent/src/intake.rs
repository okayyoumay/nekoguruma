//! The agent's intake: what a verified job instruction becomes (design 5.3, 5.6, 8.2.6, ADR-511).
//!
//! [`JobIntake`] is the [`crate::control::Intake`] the control channel hands a verified
//! instruction to. [`JobIntake::take`](crate::control::Intake::take) only queues the job and
//! returns; the channel then appends the job's `received` event and tells the intake so
//! ([`crate::control::Intake::received`]), which is when the job may be served. One task
//! ([`start`]) serves the queue one job at a time, in the order the jobs were taken, across
//! the control channel's connections (a connection that ends does not end a job, and `take` is
//! not called again for a job it was called for).
//!
//! A job is served in this order (ADR-511 item 2), and the first step that fails decides:
//!
//! 1. the start deadline, judged as the channel judges it ([`IssueMark::judge`]): a job past it
//!    ends `expired`;
//! 2. the instruction's shape: a kind other than identification names exactly one document, an
//!    identification instruction names a plan whose source ([`PlanSource`]) has it;
//! 3. the fetch of what the instruction names ([`Objects`], the [`Fetcher`] in production): a
//!    refusal the server proved ends the job as a rejected instruction, a fault of the agent's own
//!    as `preconditionFailed` with `internal`, and a server that cannot be reached, with the
//!    attempts used up, is waited for at most [`UNAVAILABLE_WAIT`] at a time until the start
//!    deadline, which then ends the job `expired`;
//! 4. the documents read back through the object source, which are exactly the set the
//!    instruction names (by [`JobDocument::sha256`]);
//! 5. the kind against the document (an `acquire`, `monitorSession` or `monitorCapture` job whose
//!    program writes is a rejected instruction), the plan against the instruction's plan hash
//!    ([`load_plan`]), and the build's policy ([`check_job`]);
//! 6. the VCI ([`VciLookup`]), its locks, its worker and the run ([`run_on_vci`]), whose cancel
//!    flag the start deadline and the agent's stop set while the job has not begun.
//!
//! A refusal by the intake is `failed` with `instructionRejected` and a detail that is empty:
//! which check refused goes to the agent's log only (ADR-511 item 3). A document or plan that
//! does not load is `preconditionFailed` with `jobRefused`, a VCI that is not found or whose
//! worker does not launch `linkNotOpened`.
//!
//! `running` is reported once, when the run's first-step hook fires. A run's end is mapped by
//! [`crate::job_report::outcome`]; nothing terminal is reported while the agent stops, except a
//! run that ended `Ok`, whose `completed` is reported and recorded (ADR-511 item 6). Only that run
//! enqueues its result in the upload ledger, before the end is recorded: the order of an end is
//! the result, the outcome record's `ended`, the outbox append, the record's `reported`
//! ([`crate::outcome_record`], ADR-511 items 7 and 8). The communication log is the run's own, at
//! the path [`crate::comm_log::comm_log_path`] gives under the journal directory; the intake does
//! not enqueue it. At its start the intake appends the ends the record holds unreported.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diag_ir::{Program, VmState};
use shared_proto::{FailureCode, JobId, JobKind, JobState};
use tokio::sync::{Notify, mpsc, watch};
use tracing::Instrument;
use worker_host::service::OperatingMode;

use crate::artifact::Artifact;
use crate::comm_log::CommLogSetup;
use crate::comm_log::{CommLogStatus, VciMargins};
use crate::control::{Intake, IntakeError, SharedOutbox, Taken};
use crate::deadline::{Clock, IssueMark, Verdict};
use crate::document::JobDocument;
use crate::fetch::{FetchError, Fetched, Fetcher, Kind as ObjectKind};
use crate::identification::{
    CheckedPlan, IdentificationReport, PlanRefused, PlanSource, check_plan, load_plan,
    run_identification,
};
use crate::inputs::ServiceSources;
use crate::instruction::VerifiedInstruction;
use crate::job_report::{self, JobOutcome, JournalStanding};
use crate::journal::{DirSource, JobKey, Vin};
use crate::journaling::JournalSetup;
use crate::link::LinkConfig;
use crate::oplog::external;
use crate::outbox::Kind as EventKind;
use crate::outcome_record::{OutcomeRecord, RecordError};
use crate::policy;
use crate::result_json::{ResultJsonError, vm_state_json};
use crate::runner::{Begin, FirstStep, Job, JobLimits, JobRun, check_job, run_program_journaled};
use crate::uploader::SharedLedger;
use crate::vci_shell::{ShellRun, VciLookup, VciShell, VciShellError, WorkerEnd, run_on_vci};

/// How many attempts one fetch makes (with the back-off between them) before the server counts
/// as unavailable for this round.
const FETCH_ATTEMPTS: u32 = 3;
/// The longest the intake waits between two rounds of a fetch the server did not answer
/// (ADR-504 item 7).
pub const UNAVAILABLE_WAIT: Duration = Duration::from_secs(60);
/// How often a waiting job looks at its start deadline.
const DEADLINE_POLL: Duration = Duration::from_secs(1);
/// How long a stop waits for a cancelled run to end, as its worker stops and its locks are
/// released, before the run is dropped: the wait for a job's end on the link, the worker's stop
/// and 5 seconds of margin.
pub(crate) const RUN_STOP_BOUND: Duration = Duration::from_secs(
    crate::link::JOB_END_WAIT_MAX.as_secs() + crate::vci_shell::STOP_GRACE.as_secs() + 5,
);

const _: () = assert!(
    RUN_STOP_BOUND.as_secs()
        > crate::link::JOB_END_WAIT_MAX.as_secs() + crate::vci_shell::STOP_GRACE.as_secs(),
    "a cancelled run's wind-down must fit in the bound with a margin"
);
/// How often an end that could not be appended is tried again while the intake is idle.
const OWED_RETRY: Duration = Duration::from_secs(60);

/// What a fetch call returns.
pub type ObjectFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, FetchError>> + Send + 'a>>;

/// Where a job's documents and artifact come from: the object cache, filled from the server. The
/// [`Fetcher`] is the production one; tests replace it.
pub trait Objects: Send + Sync {
    /// Has everything the instruction names in the cache ([`Fetcher::fetch_all`]).
    fn fetch_all<'a>(
        &'a self,
        instruction: &'a VerifiedInstruction,
        attempts: u32,
    ) -> ObjectFuture<'a, Vec<Fetched>>;

    /// The bytes of a document the instruction names ([`Fetcher::read_document`]).
    fn read_document<'a>(
        &'a self,
        instruction: &'a VerifiedInstruction,
        digest: &'a [u8; 32],
        attempts: u32,
    ) -> ObjectFuture<'a, Vec<u8>>;
}

impl Objects for Fetcher {
    fn fetch_all<'a>(
        &'a self,
        instruction: &'a VerifiedInstruction,
        attempts: u32,
    ) -> ObjectFuture<'a, Vec<Fetched>> {
        Box::pin(Fetcher::fetch_all(self, instruction, attempts))
    }

    fn read_document<'a>(
        &'a self,
        instruction: &'a VerifiedInstruction,
        digest: &'a [u8; 32],
        attempts: u32,
    ) -> ObjectFuture<'a, Vec<u8>> {
        Box::pin(Fetcher::read_document(self, instruction, digest, attempts))
    }
}

/// What the intake works with.
pub struct Parts {
    pub objects: Arc<dyn Objects>,
    pub plans: Arc<dyn PlanSource + Send + Sync>,
    pub vcis: Arc<dyn VciLookup + Send + Sync>,
    pub mode: OperatingMode,
    /// Where journals and communication logs go: the journal directory of the mode.
    pub journal_dir: DirSource,
    /// The outbox the job's events are appended to, shared with the control channel.
    pub outbox: SharedOutbox,
    pub uploads: SharedLedger,
    /// The store a job's start deadline is judged with.
    pub marks: IssueMark,
    pub clock: Arc<dyn Clock>,
    pub outcomes: OutcomeRecord,
}

impl std::fmt::Debug for Parts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parts").finish_non_exhaustive()
    }
}

/// The jobs taken and not yet served.
#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    /// A job became ready, or the queue closed.
    ready: Notify,
}

#[derive(Default)]
struct QueueState {
    /// Taken, and the channel has not yet said its `received` event is appended.
    awaiting: Vec<VerifiedInstruction>,
    /// Served in this order.
    ready: VecDeque<VerifiedInstruction>,
    /// The serving task has ended: nothing is taken any more.
    closed: bool,
}

impl Queue {
    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The intake the control channel hands instructions to.
pub struct JobIntake {
    queue: Arc<Queue>,
}

impl std::fmt::Debug for JobIntake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobIntake").finish_non_exhaustive()
    }
}

impl Intake for JobIntake {
    fn take(&self, instruction: VerifiedInstruction) -> Taken<'_> {
        let mut state = self.queue.lock();
        let result = if state.closed {
            Err(IntakeError)
        } else {
            state.awaiting.push(instruction);
            Ok(())
        };
        Box::pin(std::future::ready(result))
    }

    fn received(&self, job_id: &str, generation: u64) {
        let mut state = self.queue.lock();
        let held = state.awaiting.iter().position(|instruction| {
            let named = instruction.instruction();
            named.job_id.0 == job_id && named.ownership_generation == generation
        });
        if let Some(position) = held {
            let instruction = state.awaiting.remove(position);
            state.ready.push_back(instruction);
            drop(state);
            self.queue.ready.notify_one();
        }
    }
}

/// The running intake task, as the uploader's and the session's handles are: it stops when this
/// is dropped.
pub struct Handle {
    stop: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

impl Handle {
    /// Asks the task to stop: a job in flight is cancelled and left open, a record that is being
    /// written is awaited. A second call does nothing more.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    /// Resolves once the task has ended.
    pub async fn stopped(&self) {
        let mut done = self.done.clone();
        let _ = done.wait_for(|done| *done).await;
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

#[cfg(test)]
impl Handle {
    /// A handle of a task that has ended, for a test of what the caller does about that.
    pub(crate) fn ended_for_test() -> Handle {
        Handle {
            stop: watch::channel(false).0,
            done: watch::channel(true).1,
        }
    }
}

/// The stop signal as the task sees it. A handle that is gone is a stop too.
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

/// Says the task has ended, however it ends, and closes the queue first.
struct Ended {
    queue: Arc<Queue>,
    done: watch::Sender<bool>,
}

impl Drop for Ended {
    fn drop(&mut self) {
        self.queue.lock().closed = true;
        self.done.send_replace(true);
    }
}

/// Starts the intake task. Needs a running tokio runtime with several threads (a run needs one).
pub fn start(parts: Parts) -> (Arc<JobIntake>, Handle) {
    let queue = Arc::new(Queue::default());
    let (stop_tx, stop_rx) = watch::channel(false);
    let (done_tx, done_rx) = watch::channel(false);
    let worker = Worker {
        parts,
        queue: Arc::clone(&queue),
        stop: Stop(stop_rx),
        appended: Mutex::default(),
        warned: Mutex::default(),
        in_memory: Mutex::default(),
    };
    let ended = Ended {
        queue: Arc::clone(&queue),
        done: done_tx,
    };
    tokio::spawn(async move {
        let _ended = ended;
        worker.run().await;
    });
    (
        Arc::new(JobIntake { queue }),
        Handle {
            stop: stop_tx,
            done: done_rx,
        },
    )
}

/// How a job's handling ended.
#[derive(Debug)]
enum Ending {
    /// The job ends in this terminal state; `result` is what an `Ok` run enqueues.
    Ended {
        state: JobState,
        result: Option<Vec<u8>>,
    },
    /// Nothing terminal is reported: the agent stops, or the outcome says the job is open or
    /// another run's.
    Open,
}

impl Ending {
    fn state(state: JobState) -> Self {
        Ending::Ended {
            state,
            result: None,
        }
    }

    fn expired() -> Self {
        Ending::state(JobState::Expired)
    }

    /// A refusal by the intake: the check that refused goes to the log only (ADR-511 item 3).
    fn rejected(why: &'static str) -> Self {
        tracing::warn!(why, "the intake refused the instruction");
        Ending::state(JobState::Failed {
            code: FailureCode::InstructionRejected,
            detail: String::new(),
            procedure_code: None,
        })
    }

    /// A failure before the run: `why` is the detail and the log's word, the cause goes to the log.
    fn precondition(
        code: FailureCode,
        why: &'static str,
        cause: &(dyn std::error::Error + 'static),
    ) -> Self {
        tracing::warn!(why, external = external(cause), "the job cannot start");
        Ending::state(JobState::PreconditionFailed {
            code,
            detail: why.to_owned(),
        })
    }
}

/// What a failed fetch round means for the job.
#[derive(Debug, PartialEq, Eq)]
enum FetchClass {
    /// The server proved its refusal of an object.
    Refused,
    /// The agent's own fault.
    Local,
    /// The server did not answer for this round; wait and fetch again.
    Unavailable,
}

fn fetch_class(error: &FetchError) -> FetchClass {
    match error {
        FetchError::Refused { .. } => FetchClass::Refused,
        FetchError::Local { .. } | FetchError::Task(_) | FetchError::NotNamed => FetchClass::Local,
        FetchError::Retry { .. }
        | FetchError::ExchangeCapped
        | FetchError::Unproven { .. }
        | FetchError::Mismatch
        | FetchError::MismatchTwice => FetchClass::Unavailable,
    }
}

/// Where a job stands against its start deadline. The job's first-step hook and the deadline each
/// move it out of `WAITING` by a compare-and-swap, so exactly one of "began" and "cut" happens
/// (ADR-511 item 5).
struct StartGate(AtomicU8);

impl StartGate {
    const WAITING: u8 = 0;
    const BEGUN: u8 = 1;
    const CUT: u8 = 2;

    fn new() -> Self {
        Self(AtomicU8::new(Self::WAITING))
    }

    /// The job's first step: the job begins unless the deadline cut it first.
    fn begin(&self) -> Begin {
        match self.0.compare_exchange(
            Self::WAITING,
            Self::BEGUN,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Begin::Go,
            Err(_) => Begin::Expired,
        }
    }

    /// The start deadline: true when the job had not begun, so that it never will.
    fn cut(&self) -> bool {
        self.0
            .compare_exchange(
                Self::WAITING,
                Self::CUT,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

/// The hook a run calls at its first step: it asks `gate` whether the job may begin, and reports
/// the beginning on `first_step`.
fn first_step_hook(gate: Arc<StartGate>, first_step: mpsc::UnboundedSender<()>) -> FirstStep {
    Box::new(move || {
        let begin = gate.begin();
        if begin == Begin::Go {
            // The job thread does not wait: a send on an unbounded channel does not block.
            let _ = first_step.send(());
        }
        begin
    })
}

/// What watches a run that has not yet begun: the flag that cancels the wait for its locks, the
/// flag that cancels the job, the channel its first step reports on, the gate that decides
/// between its first step and the start deadline, and the start deadline.
struct Watched {
    gate: Arc<StartGate>,
    /// Ends the shell's wait for the locks and its launch: the start deadline and the stop set it.
    lock_cancel: Arc<AtomicBool>,
    /// Cancels the job itself: only a stop sets it.
    job_cancel: Arc<AtomicBool>,
    started: mpsc::UnboundedReceiver<()>,
    deadline: u64,
}

/// What a job is run as, after the checks.
enum Prepared {
    Program {
        job: Box<Job>,
        link: LinkConfig,
        writes: bool,
        /// The program, for the journal's standing after the run.
        program: Box<Program>,
    },
    Identification {
        plan: Box<CheckedPlan>,
    },
}

/// What failed about a job's end, for logging each kind once per job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FailureKind {
    /// The `reported` record was not written.
    RecordReported,
    /// The append to the outbox failed.
    Append,
    /// Neither write worked and the end is kept in memory.
    KeptInMemory,
}

/// The task's state.
struct Worker {
    parts: Parts,
    queue: Arc<Queue>,
    stop: Stop,
    /// Ends the record holds unreported that were appended in this start: appended once per start,
    /// whatever becomes of their `reported` record.
    appended: Mutex<HashSet<(String, u64)>>,
    /// The failures logged at their first level, by job and kind: the repeats are `debug`.
    warned: Mutex<HashSet<(String, u64, FailureKind)>>,
    /// Ends neither recorded nor appended (both writes failed): kept here, tried again, and lost
    /// only if the agent ends first.
    in_memory: Mutex<Vec<(JobKey, JobState)>>,
}

impl Worker {
    async fn run(self) {
        loop {
            if self.stop.is_set() {
                break;
            }
            // The ends that could not be appended before are tried again whenever the task comes
            // here: at its start, and between two jobs.
            self.recover().await;
            let next = tokio::select! {
                biased;
                () = self.stop.requested() => break,
                next = self.next_job() => next,
            };
            let key = job_key(&next);
            let span = crate::logging::job_span(&key, &next.instruction().vci.0);
            self.serve(next, &key).instrument(span).await;
        }
    }

    /// The next job the channel has released, waiting for one. While it waits, ends that could not
    /// be appended are tried again.
    async fn next_job(&self) -> VerifiedInstruction {
        loop {
            let ready = self.queue.ready.notified();
            if let Some(next) = self.queue.lock().ready.pop_front() {
                return next;
            }
            let owed = !self.parts.outcomes.owed().is_empty()
                || !self
                    .in_memory
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty();
            tokio::select! {
                () = ready => {}
                () = tokio::time::sleep(OWED_RETRY), if owed => self.recover().await,
            }
        }
    }

    /// Appends the ends the outcome record holds unreported (ADR-511 item 7), each once per start,
    /// and the ends kept in memory because neither write of them worked.
    async fn recover(&self) {
        let in_memory =
            std::mem::take(&mut *self.in_memory.lock().unwrap_or_else(|e| e.into_inner()));
        for (key, state) in in_memory {
            let span = crate::logging::job_span(&key, "");
            let recorded = self.record_end(&key, &state).instrument(span.clone()).await;
            if !self.announce(&key, &state).instrument(span).await && !recorded {
                self.keep_in_memory(key, state);
            }
        }
        for (key, state) in self.parts.outcomes.owed() {
            let span = crate::logging::job_span(&key, "");
            let id = (key.job_id.0.clone(), key.generation);
            let appended = self
                .appended
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&id);
            if appended {
                // In the outbox already: only the record of it is missing.
                self.record_reported(&key).instrument(span).await;
            } else {
                self.announce(&key, &state).instrument(span).await;
            }
        }
    }

    fn keep_in_memory(&self, key: JobKey, state: JobState) {
        let first = self.first_failure(&key, FailureKind::KeptInMemory);
        let text = "the job's end is neither recorded nor appended; it is kept in memory and tried again, and is lost if the agent ends first";
        if first {
            tracing::error!("{text}");
        } else {
            tracing::debug!("{text}");
        }
        self.in_memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((key, state));
    }

    async fn serve(&self, instruction: VerifiedInstruction, key: &JobKey) {
        let instruction = Arc::new(instruction);
        let ending = self.handle(&instruction, key).await;
        self.finish(key, ending).await;
    }

    async fn handle(&self, instruction: &Arc<VerifiedInstruction>, key: &JobKey) -> Ending {
        let named = instruction.instruction();
        let deadline = named.start_deadline;
        if self.stop.is_set() {
            return Ending::Open;
        }
        if self.expired(deadline) {
            return Ending::expired();
        }
        let plan_bytes = match shape(instruction, &*self.parts.plans) {
            Ok(plan) => plan,
            Err(ending) => return ending,
        };
        let fetched = match self.fetch(instruction, deadline).await {
            Ok(fetched) => fetched,
            Err(ending) => return ending,
        };
        let (documents, raw) = match self
            .read_documents(instruction, plan_bytes.is_some(), deadline)
            .await
        {
            Ok(held) => held,
            Err(ending) => return ending,
        };
        let artifact = match self.read_artifact(instruction, &fetched).await {
            Ok(artifact) => artifact,
            Err(ending) => return ending,
        };
        // The checks load the documents and the plan, which is work for a blocking thread.
        let checked = {
            let instruction = Arc::clone(instruction);
            let span = tracing::Span::current();
            tokio::task::spawn_blocking(move || {
                span.in_scope(|| prepare(&instruction, documents, plan_bytes, &raw, artifact))
            })
            .await
        };
        let prepared = match checked {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(ending)) => return ending,
            Err(_) => return Ending::panicked(),
        };
        self.run_job(instruction, key, prepared, deadline).await
    }

    /// Whether the start deadline has passed, judged as the channel judges it.
    fn expired(&self, deadline: u64) -> bool {
        matches!(
            self.parts.marks.judge(&*self.parts.clock, deadline),
            Verdict::Expired { .. }
        )
    }

    /// Resolves once the start deadline has passed.
    async fn deadline_passed(&self, deadline: u64) {
        while !self.expired(deadline) {
            tokio::time::sleep(DEADLINE_POLL).await;
        }
    }

    /// Fetches what the instruction names, again after each round the server did not answer,
    /// until the start deadline (ADR-511 items 2 and 5).
    async fn fetch(
        &self,
        instruction: &VerifiedInstruction,
        deadline: u64,
    ) -> Result<Vec<Fetched>, Ending> {
        loop {
            let round = self.parts.objects.fetch_all(instruction, FETCH_ATTEMPTS);
            let error = tokio::select! {
                biased;
                () = self.stop.requested() => return Err(Ending::Open),
                () = self.deadline_passed(deadline) => return Err(Ending::expired()),
                done = round => match done {
                    Ok(fetched) => return Ok(fetched),
                    Err(error) => error,
                },
            };
            match fetch_class(&error) {
                FetchClass::Refused => {
                    tracing::warn!(external = external(&error), "an object was refused");
                    return Err(Ending::rejected("the server refused an object"));
                }
                FetchClass::Local => {
                    return Err(Ending::precondition(
                        FailureCode::Internal,
                        "an object cannot be fetched",
                        &error,
                    ));
                }
                FetchClass::Unavailable => {
                    tracing::warn!(
                        class = error.class(),
                        "the server did not give the job's objects; trying again"
                    );
                }
            }
            self.wait_unavailable(deadline).await?;
        }
    }

    /// The wait between two rounds with a server that did not answer (ADR-504 item 7); the stop
    /// and the start deadline end it.
    async fn wait_unavailable(&self, deadline: u64) -> Result<(), Ending> {
        tokio::select! {
            biased;
            () = self.stop.requested() => Err(Ending::Open),
            () = self.deadline_passed(deadline) => Err(Ending::expired()),
            () = tokio::time::sleep(UNAVAILABLE_WAIT) => Ok(()),
        }
    }

    /// The documents the instruction names, read through the object source and nowhere else, and
    /// checked to be exactly the named set. `keep_bytes` keeps the bytes of each, which a plan is
    /// loaded from. A read the server did not answer is waited for as a fetch is.
    async fn read_documents(
        &self,
        instruction: &VerifiedInstruction,
        keep_bytes: bool,
        deadline: u64,
    ) -> Result<(Vec<JobDocument>, Vec<([u8; 32], Vec<u8>)>), Ending> {
        let named = &instruction.instruction().document_hashes;
        let mut documents = Vec::with_capacity(named.len());
        let mut raw = Vec::new();
        for digest in named {
            let bytes = loop {
                let read = self
                    .parts
                    .objects
                    .read_document(instruction, digest, FETCH_ATTEMPTS);
                let error = tokio::select! {
                    biased;
                    () = self.stop.requested() => return Err(Ending::Open),
                    () = self.deadline_passed(deadline) => return Err(Ending::expired()),
                    bytes = read => match bytes {
                        Ok(bytes) => break bytes,
                        Err(error) => error,
                    },
                };
                match fetch_class(&error) {
                    FetchClass::Refused => {
                        tracing::warn!(external = external(&error), "a document was refused");
                        return Err(Ending::rejected("the server refused a document"));
                    }
                    FetchClass::Local => {
                        return Err(Ending::precondition(
                            FailureCode::Internal,
                            "a document cannot be read",
                            &error,
                        ));
                    }
                    FetchClass::Unavailable => {
                        tracing::warn!(
                            class = error.class(),
                            "the server did not give a document; trying again"
                        );
                        self.wait_unavailable(deadline).await?;
                    }
                }
            };
            // A plan is loaded from the bytes, so they are kept beside the document that is
            // built from them: at most one more copy of a document (16 MiB at most) per job. The
            // load checks the whole document, so it runs on a blocking thread.
            let built = tokio::task::spawn_blocking(move || {
                let kept = keep_bytes.then(|| bytes.clone());
                JobDocument::new(bytes).map(|document| (document, kept))
            })
            .await;
            let (document, kept) = match built {
                Ok(Ok(built)) => built,
                Ok(Err(error)) => {
                    return Err(Ending::precondition(
                        FailureCode::JobRefused,
                        "a document does not load",
                        &error,
                    ));
                }
                Err(_) => return Err(Ending::panicked()),
            };
            if let Some(bytes) = kept {
                raw.push((*digest, bytes));
            }
            documents.push(document);
        }
        if !held_equals_named(named, &documents) {
            return Err(Ending::precondition(
                FailureCode::Internal,
                "the documents held are not the documents named",
                &HeldSetDiffers,
            ));
        }
        Ok((documents, raw))
    }

    /// The artifact the instruction names, from the cache, when it names one.
    async fn read_artifact(
        &self,
        instruction: &VerifiedInstruction,
        fetched: &[Fetched],
    ) -> Result<Option<([u8; 32], Artifact)>, Ending> {
        let Some(hash) = instruction.instruction().artifact_hash else {
            return Ok(None);
        };
        let Some(path) = fetched
            .iter()
            .find(|object| object.kind == ObjectKind::Artifact)
            .map(|object| object.path.clone())
        else {
            return Err(Ending::precondition(
                FailureCode::Internal,
                "the artifact is not in the cache",
                &ArtifactMissing,
            ));
        };
        let read = tokio::task::spawn_blocking(move || Artifact::read(&path));
        let artifact = tokio::select! {
            biased;
            () = self.stop.requested() => return Err(Ending::Open),
            read = read => read,
        };
        match artifact {
            Ok(Ok(artifact)) => Ok(Some((hash, artifact))),
            // The agent's own cache: a read that fails is its fault, not the job's data.
            Ok(Err(error)) => Err(Ending::precondition(
                FailureCode::Internal,
                "the artifact cannot be read",
                &error,
            )),
            Err(error) => Err(Ending::precondition(
                FailureCode::Internal,
                "the artifact's read failed",
                &error,
            )),
        }
    }

    /// Finds the VCI, runs the job on it and maps the end.
    async fn run_job(
        &self,
        instruction: &VerifiedInstruction,
        key: &JobKey,
        prepared: Prepared,
        deadline: u64,
    ) -> Ending {
        let named = instruction.instruction();
        // The lookup reads the registration definitions, the directory is probed and the log's
        // directory is made: one blocking section.
        let identifies = matches!(prepared, Prepared::Identification { .. });
        let (vcis, journal_dir) = (
            Arc::clone(&self.parts.vcis),
            Arc::clone(&self.parts.journal_dir),
        );
        let (vci, log_key, span) = (named.vci.0.clone(), key.clone(), tracing::Span::current());
        let found = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let route = vcis.find(&vci).map_err(|error| shell_ending(&error))?;
                let dir = journal_dir().map_err(|error| {
                    Ending::precondition(
                        FailureCode::Internal,
                        "the journal directory is unusable",
                        &error,
                    )
                })?;
                let comm_log = identifies
                    .then(|| CommLogSetup::for_run_key(dir.clone(), log_key))
                    .transpose()
                    .map_err(|error| {
                        Ending::precondition(
                            FailureCode::Internal,
                            "the communication log's directory is unusable",
                            &error,
                        )
                    })?;
                Ok((route, dir, comm_log))
            })
        })
        .await;
        let (route, dir, comm_log) = match found {
            Ok(Ok(found)) => found,
            Ok(Err(ending)) => return ending,
            Err(_) => return Ending::panicked(),
        };
        let shell = VciShell {
            vci: named.vci.0.clone(),
            mode: self.parts.mode,
            route,
        };
        let lock_cancel = Arc::new(AtomicBool::new(false));
        let job_cancel = Arc::new(AtomicBool::new(false));
        let (first_step, started) = mpsc::unbounded_channel::<()>();
        let gate = Arc::new(StartGate::new());
        let first = first_step_hook(Arc::clone(&gate), first_step);
        let watched = Watched {
            gate,
            lock_cancel: Arc::clone(&lock_cancel),
            job_cancel: Arc::clone(&job_cancel),
            started,
            deadline,
        };
        match prepared {
            Prepared::Program {
                job,
                link,
                writes,
                program,
            } => {
                let setup = JournalSetup {
                    dir: dir.clone(),
                    key: key.clone(),
                    sources: ServiceSources::default(),
                    vin: named.vin.as_ref().map(|vin| Vin::new(vin.0.clone())),
                    intended_software_version: None,
                };
                let job = job.with_cancel(job_cancel);
                let run = run_on_vci(
                    shell,
                    writes,
                    lock_cancel,
                    move |source, vci, guards| async move {
                        let job = job.with_vci(vci);
                        let JobRun {
                            result,
                            guards,
                            comm_log,
                        } = run_program_journaled(
                            source,
                            &link,
                            job,
                            JobLimits::default(),
                            setup,
                            guards,
                            Some(first),
                        )
                        .await;
                        (result, guards, comm_log)
                    },
                );
                let standing_key = key.clone();
                self.supervise(
                    key,
                    run,
                    watched,
                    move || JournalStanding::read(&dir, &standing_key, &program, true),
                    |state: &VmState| vm_state_json(state),
                )
                .await
            }
            Prepared::Identification { plan } => {
                let comm_log =
                    comm_log.expect("the section above made the log of an identification");
                let run = run_on_vci(
                    shell,
                    false,
                    lock_cancel,
                    move |source, vci, guards| async move {
                        let run = run_identification(
                            source,
                            *plan,
                            JobLimits::default(),
                            guards,
                            Some(comm_log),
                            Some(vci),
                            // No VCI profile gives the agent margins.
                            VciMargins::default(),
                            job_cancel,
                            Some(first),
                        )
                        .await;
                        (run.result, run.guards, run.comm_log)
                    },
                );
                // An identification run keeps no journal.
                self.supervise(
                    key,
                    run,
                    watched,
                    || JournalStanding::Clean,
                    |report: &IdentificationReport| {
                        serde_json::to_string(report).map_err(ResultJsonError::Encode)
                    },
                )
                .await
            }
        }
    }

    /// Runs `run` as a task of its own and watches it: `running` once its first step begins, the
    /// start deadline while it has not (it cancels the wait for the locks and the launch, and
    /// the first step then refuses to begin; a job that has begun is not ended by it), the stop at
    /// any time. A stop cancels the job and the
    /// shell, and the run is awaited for [`RUN_STOP_BOUND`] so that its worker stops, its locks are
    /// released and a run that ended `Ok` is reported; only a run that does not end in time is
    /// dropped, and its job stays open.
    async fn supervise<T: Send + 'static>(
        &self,
        key: &JobKey,
        run: impl Future<Output = Result<ShellRun<T>, VciShellError>> + Send + 'static,
        watched: Watched,
        standing: impl FnOnce() -> JournalStanding + Send + 'static,
        to_json: impl FnOnce(&T) -> Result<String, ResultJsonError>,
    ) -> Ending {
        let Watched {
            gate,
            lock_cancel,
            job_cancel,
            mut started,
            deadline,
        } = watched;
        let mut run = tokio::spawn(run);
        let mut announced = false;
        let mut watching = true;
        let mut cut = false;
        let mut deadline_seen = false;
        let mut stopping = false;
        let mut dropped = false;
        let deadline_watch = self.deadline_passed(deadline);
        tokio::pin!(deadline_watch);
        let stop_bound = tokio::time::sleep(RUN_STOP_BOUND);
        tokio::pin!(stop_bound);
        let joined = loop {
            tokio::select! {
                biased;
                joined = &mut run => break joined,
                message = started.recv(), if watching => {
                    watching = false;
                    if message.is_some() {
                        announced = true;
                        self.report_running(key).await;
                    }
                }
                () = &mut deadline_watch, if !deadline_seen => {
                    deadline_seen = true;
                    // A job that has begun is past its deadline's reach.
                    cut = gate.cut();
                    if cut {
                        lock_cancel.store(true, Ordering::Relaxed);
                    }
                }
                () = self.stop.requested(), if !stopping => {
                    stopping = true;
                    lock_cancel.store(true, Ordering::Relaxed);
                    job_cancel.store(true, Ordering::Relaxed);
                    stop_bound.as_mut().reset(tokio::time::Instant::now() + RUN_STOP_BOUND);
                }
                () = &mut stop_bound, if stopping && !dropped => {
                    dropped = true;
                    tracing::warn!("the run did not end within the bound of a stop; it is dropped and the job stays open");
                    run.abort();
                }
            }
        };
        // A run that ended before the loop saw its first step still began: `running` comes
        // before its end.
        if !announced && started.try_recv().is_ok() {
            self.report_running(key).await;
        }
        match joined {
            Err(error) if error.is_cancelled() => Ending::Open,
            Err(_) => Ending::panicked(),
            Ok(Err(VciShellError::Cancelled)) => {
                if cut && !stopping {
                    Ending::expired()
                } else {
                    Ending::Open
                }
            }
            Ok(Err(error)) => shell_ending(&error),
            Ok(Ok(run)) => {
                let ShellRun {
                    result,
                    comm_log,
                    worker_end,
                } = run;
                log_run_end(&comm_log, &worker_end);
                // The journal is read on a blocking thread.
                let span = tracing::Span::current();
                // The standing does not decide an `Ok` run's outcome, so a read that fails
                // leaves a finished run `completed`.
                let standing = match tokio::task::spawn_blocking(move || span.in_scope(standing))
                    .await
                {
                    Ok(standing) => standing,
                    Err(error) if result.is_ok() => {
                        tracing::warn!(
                            error = &error as &dyn std::error::Error,
                            "the journal's standing could not be read; the finished run is reported all the same"
                        );
                        JournalStanding::Clean
                    }
                    Err(_) => return Ending::panicked(),
                };
                match job_report::outcome(&result, true, &standing) {
                    JobOutcome::Terminal(JobState::Completed) => {
                        let value = result.as_ref().ok();
                        let bytes = value.map(to_json).and_then(|json| match json {
                            Ok(json) => Some(json.into_bytes()),
                            Err(error) => {
                                tracing::warn!(
                                    error = &error as &dyn std::error::Error,
                                    "the job's result cannot be written; none is enqueued"
                                );
                                None
                            }
                        });
                        Ending::Ended {
                            state: JobState::Completed,
                            result: bytes,
                        }
                    }
                    JobOutcome::Terminal(state) => Ending::state(state),
                    JobOutcome::Open | JobOutcome::None => Ending::Open,
                }
            }
        }
    }

    /// Reports `running` once; a failure is the log's.
    async fn report_running(&self, key: &JobKey) {
        if let Err(error) = self
            .parts
            .outbox
            .append_job_state(EventKind::Other, key.job_id.0.clone(), JobState::Running)
            .await
        {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "the running event was not appended"
            );
        }
    }

    /// The end of a job: the result (for `Ok`), the record's `ended`, the outbox append, the
    /// record's `reported`. While the agent stops nothing terminal is reported, except `completed`.
    /// An end that is neither recorded nor appended is kept in memory and tried again.
    async fn finish(&self, key: &JobKey, ending: Ending) {
        let Ending::Ended { state, result } = ending else {
            return;
        };
        if self.stop.is_set() && state != JobState::Completed {
            return;
        }
        if let Some(bytes) = result
            && let Err(error) = self.parts.uploads.enqueue_result(key.clone(), bytes).await
        {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "the job's result was not enqueued; its state is reported all the same"
            );
        }
        let recorded = self.record_end(key, &state).await;
        if !self.announce(key, &state).await && !recorded {
            self.keep_in_memory(key.clone(), state);
        }
    }

    /// Makes the end durable in the outcome record; false when that failed.
    async fn record_end(&self, key: &JobKey, state: &JobState) -> bool {
        let (ended_key, ended_state) = (key.clone(), state.clone());
        match self
            .record(move |record| record.ended(&ended_key, &ended_state))
            .await
        {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    "the job's end was not recorded; it is reported all the same"
                );
                false
            }
        }
    }

    /// Records that the end of `key` is in the outbox. A failure is logged once and the end is not
    /// appended again in this start.
    async fn record_reported(&self, key: &JobKey) {
        let reported = key.clone();
        if let Err(error) = self.record(move |record| record.reported(&reported)).await {
            self.log_failure(
                key,
                FailureKind::RecordReported,
                &error,
                "the job's report was not recorded; it is appended again at the next start",
            );
        }
    }

    /// Whether this is the first failure of `kind` for the job in this start.
    fn first_failure(&self, key: &JobKey, kind: FailureKind) -> bool {
        self.warned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((key.job_id.0.clone(), key.generation, kind))
    }

    /// Logs a failure at `warn` the first time for a job and kind in this start and at `debug`
    /// after.
    fn log_failure(
        &self,
        key: &JobKey,
        kind: FailureKind,
        error: &(dyn std::error::Error + 'static),
        text: &str,
    ) {
        if self.first_failure(key, kind) {
            tracing::warn!(error, "{text}");
        } else {
            tracing::debug!(error, "{text}");
        }
    }

    /// Appends the terminal `state` to the outbox and records that it is there; true when the
    /// append worked. An append that fails leaves the end owed (in the record, or in memory).
    async fn announce(&self, key: &JobKey, state: &JobState) -> bool {
        match self
            .parts
            .outbox
            .append_job_state(EventKind::Other, key.job_id.0.clone(), state.clone())
            .await
        {
            Ok(_) => {
                self.appended
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert((key.job_id.0.clone(), key.generation));
                self.record_reported(key).await;
                true
            }
            Err(error) => {
                self.log_failure(
                    key,
                    FailureKind::Append,
                    &error,
                    "the job's end was not appended; it is tried again",
                );
                false
            }
        }
    }

    /// A call on the outcome record on a blocking task; a poisoned record is opened again first.
    async fn record<R: Send + 'static>(
        &self,
        call: impl FnOnce(&OutcomeRecord) -> Result<R, RecordError> + Send + 'static,
    ) -> Result<R, RecordError> {
        let record = self.parts.outcomes.clone();
        tokio::task::spawn_blocking(move || {
            record.reopen_if_poisoned()?;
            call(&record)
        })
        .await
        .unwrap_or(Err(RecordError::Io(std::io::ErrorKind::Other)))
    }
}

fn job_key(instruction: &VerifiedInstruction) -> JobKey {
    let named = instruction.instruction();
    JobKey {
        job_id: JobId(named.job_id.0.clone()),
        generation: named.ownership_generation,
    }
}

/// The checks of the instruction's own shape, before any fetch (ADR-511 item 2.1): the plan's
/// bytes for an identification instruction.
fn shape(
    instruction: &VerifiedInstruction,
    plans: &dyn PlanSource,
) -> Result<Option<Vec<u8>>, Ending> {
    let named = instruction.instruction();
    if named.kind != JobKind::Identify {
        return match named.document_hashes.len() {
            1 => Ok(None),
            _ => Err(Ending::rejected(
                "an instruction other than identification must name exactly one document",
            )),
        };
    }
    let Some(plan_hash) = named.plan_hash else {
        return Err(Ending::rejected(
            "an identification instruction names no plan hash",
        ));
    };
    match plans.plan(&plan_hash) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) => {
            tracing::warn!(external = external(&error), "the plan source gave no plan");
            Err(Ending::rejected(
                "the agent has no plan the instruction names",
            ))
        }
    }
}

/// Whether the documents held are exactly the documents named: every held document's hash is
/// named and every named hash is held.
fn held_equals_named(named: &[[u8; 32]], held: &[JobDocument]) -> bool {
    let mut held: Vec<[u8; 32]> = held.iter().map(JobDocument::sha256).collect();
    held.sort_unstable();
    let mut named = named.to_vec();
    named.sort_unstable();
    held == named
}

#[derive(Debug, thiserror::Error)]
#[error("the documents read are not the set the instruction names")]
struct HeldSetDiffers;

#[derive(Debug, thiserror::Error)]
#[error("the fetch gave no artifact the instruction names")]
struct ArtifactMissing;

/// The checks that follow the fetch (ADR-511 item 2.4 and 2.5): the kind against the document,
/// the plan against the plan hash, the build's policy.
fn prepare(
    instruction: &VerifiedInstruction,
    mut documents: Vec<JobDocument>,
    plan_bytes: Option<Vec<u8>>,
    raw: &[([u8; 32], Vec<u8>)],
    artifact: Option<([u8; 32], Artifact)>,
) -> Result<Prepared, Ending> {
    let named = instruction.instruction();
    if let Some(plan_bytes) = plan_bytes {
        let loaded = load_plan(&plan_bytes, raw, Some(instruction)).map_err(|error| {
            match error {
                // The instruction vouches for another plan than the one the source gave.
                PlanRefused::PlanHashMismatch
                | PlanRefused::InstructionNamesNoPlan
                | PlanRefused::InstructionNotIdentify => {
                    tracing::warn!(external = external(&error), "the plan is not the named one");
                    Ending::rejected("the plan's hash differs from the instruction's")
                }
                other => {
                    Ending::precondition(FailureCode::JobRefused, "the plan does not load", &other)
                }
            }
        })?;
        let plan = check_plan(&loaded).map_err(|error| {
            Ending::precondition(FailureCode::JobRefused, "the plan is refused", &error)
        })?;
        return Ok(Prepared::Identification {
            plan: Box::new(plan),
        });
    }
    let document = documents
        .pop()
        .expect("the shape check left exactly one document for a program job");
    let mut job = Job::from_document(document).map_err(|error| {
        Ending::precondition(
            FailureCode::JobRefused,
            "the document does not load",
            &error,
        )
    })?;
    // The program writes, or is not provably read-only: a kind that is read-only refuses it,
    // before the policy and before any link (ADR-511 item 3).
    let writes = job.writes();
    if writes
        && matches!(
            named.kind,
            JobKind::Acquire | JobKind::MonitorSession | JobKind::MonitorCapture
        )
    {
        return Err(Ending::rejected(
            "a read-only job names a document that writes",
        ));
    }
    if let Some((hash, artifact)) = artifact {
        job = job.with_artifact(hash, artifact);
    }
    if let Err(error) = check_job(&job, policy::build_ceiling()) {
        // The job's first run that fails before anything is sent: the mapping of the ending is
        // the run's (ADR-475).
        return Err(
            match job_report::outcome::<VmState>(&Err(error), true, &JournalStanding::Clean) {
                JobOutcome::Terminal(state) => Ending::state(state),
                JobOutcome::Open | JobOutcome::None => Ending::Open,
            },
        );
    }
    let link = job.declared_link().map_err(|error| {
        Ending::precondition(
            FailureCode::JobRefused,
            "the document declares no usable link",
            &error,
        )
    })?;
    // One program per job, copied for the journal's standing after the run.
    let program = Box::new(job.program().clone());
    Ok(Prepared::Program {
        job: Box::new(job),
        link,
        writes,
        program,
    })
}

/// How a failure of the VCI shell ends the job.
fn shell_ending(error: &VciShellError) -> Ending {
    match error {
        VciShellError::UnknownVci { .. }
        | VciShellError::AmbiguousVci { .. }
        | VciShellError::Lookup { .. }
        | VciShellError::Launch(_) => Ending::precondition(
            FailureCode::LinkNotOpened,
            "the VCI cannot be reached",
            error,
        ),
        VciShellError::Guards(_) | VciShellError::GuardsTask(_) => Ending::precondition(
            FailureCode::Internal,
            "the job's locks cannot be taken",
            error,
        ),
        // The callers that can see a cancel decide what it means.
        VciShellError::Cancelled => Ending::Open,
    }
}

/// What the log says of how a run's worker and communication log ended.
fn log_run_end(comm_log: &CommLogStatus, worker_end: &WorkerEnd) {
    if let CommLogStatus::Incomplete(incomplete) = comm_log {
        tracing::warn!(?incomplete, "the communication log is incomplete");
    }
    match worker_end {
        WorkerEnd::Exited => {}
        WorkerEnd::Killed => tracing::warn!("the worker did not stop in time and was killed"),
        WorkerEnd::NotClean(error) => tracing::warn!(
            external = external(error),
            "the worker did not stop cleanly"
        ),
        WorkerEnd::MayStillHold(error) => tracing::error!(
            external = external(error),
            "the worker may still hold the VCI; its locks stay held"
        ),
    }
}

impl Ending {
    /// A run task that panicked.
    fn panicked() -> Self {
        // The panic's own message is not logged: it may carry text of the job's data.
        tracing::error!("the job's run task panicked");
        Ending::state(JobState::Failed {
            code: FailureCode::Internal,
            detail: "the run task failed".to_owned(),
            procedure_code: None,
        })
    }
}

#[cfg(test)]
mod tests;

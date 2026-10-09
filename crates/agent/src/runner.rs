//! Runs a procedure on a link (ADR-235).
//!
//! No server and no section policy. A host error ends the job; the VM state then still points
//! at the failed primitive (ADR-233 item 1). [`run_program_journaled`] also writes the
//! write-job journal at a flash recovery plan's boundaries (`journaling`, ADR-252), and
//! [`resume_program_journaled`] goes on with a job whose journal exists (`restart`, ADR-255).

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diag_ir::{
    DiagHost, IdentityKind, Program, StepError, StepOutcome, Vm, VmError, VmState, WaitingOn,
};
use tokio::runtime::{Handle, RuntimeFlavor};
use worker_host::client::WorkerClient;

use crate::guards::{GuardError, JobGuards};
use crate::host::{HostError, Timings, TransferProgress, WorkerHost};
use crate::inputs::RuntimeInputs;
use crate::journal::{Journal, JournalError, StepRef, Store};
use crate::journaling::{JobJournal, JournalSetup};
use crate::link::{self, LinkConfig};
use crate::policy::{self, Permission};
use crate::restart::{self, OnSiteReason, RestartDecision};

/// Bounds of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    /// Steps after which the job is stopped, against a procedure that never ends.
    pub max_steps: u64,
    /// Pause between polls of a `Wait` instruction, between attempts of a restart's wait for
    /// the per-vehicle lock (ADR-263), and between the cancel checks of a restart's passive
    /// teardown (ADR-264), so it also bounds how late those waits see a cancel; at least 1 ms is
    /// used.
    pub wait_poll: Duration,
}

impl Default for JobLimits {
    fn default() -> Self {
        Self {
            max_steps: 1_000_000,
            wait_poll: Duration::from_millis(10),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("the runner needs a multi-threaded Tokio runtime")]
    CurrentThreadRuntime,
    #[error("link: {0}")]
    Link(#[source] HostError),
    #[error("procedure failed at {pc}: {source}")]
    Vm {
        pc: u32,
        #[source]
        source: VmError,
    },
    #[error("the program is refused: {0}")]
    Program(#[from] diag_ir::ProgramError),
    #[error("primitive failed at {pc}: {source}")]
    Host {
        pc: u32,
        #[source]
        source: HostError,
    },
    #[error("the procedure is waiting on {0:?}, which this agent cannot answer")]
    Unanswerable(WaitingOn),
    #[error("the procedure ran {0} steps without finishing")]
    StepLimit(u64),
    #[error("instruction {pc} is refused: {source}")]
    Refused {
        pc: u32,
        #[source]
        source: HostError,
    },
    #[error("the job was cancelled")]
    Cancelled,
    #[error("journal: {0}")]
    Journal(#[from] JournalError),
    #[error("the ECU's {identity:?} could not be read at {pc}, so the plan does not start")]
    IdentityUnreadable { pc: u32, identity: IdentityKind },
    #[error("reading the ECU's {identity:?} at {pc} failed: {source}")]
    IdentityRead {
        pc: u32,
        identity: IdentityKind,
        #[source]
        source: HostError,
    },
    /// The ECU's identity decodes to another vehicle or ECU than the job's; the job is aborted
    /// (design 5.6 Interrupted -> Failed, ADR-229 item 2). The message carries no identity
    /// value (design 16.2).
    #[error("the ECU's {identity:?} is not the job's, so the job is aborted")]
    IdentityMismatch { identity: IdentityKind },
    /// The job must not go on automatically (design 5.6, 8.2.5, 8.10.1): it ends here and
    /// waits for someone on site.
    #[error("the job needs on-site intervention: {0:?}")]
    OnSiteInterventionRequired(OnSiteReason),
    #[error("the program writes, but its guards do not hold the device's reprogramming slot")]
    NoReprogrammingSlot,
    #[error(
        "the job's guards hold a link that was not confirmed closed; stop the worker and call \
         `JobGuards::worker_gone` first"
    )]
    LinkUnconfirmed,
    #[error("the job thread panicked")]
    Panicked,
    /// A restart could not take the per-vehicle lock right after the ECU's VIN matched
    /// (ADR-263): the lock file failed or the guards cannot take a vehicle (an empty guard slot
    /// is [`JobError::GuardsMissing`]). Only ReadDataByIdentifier requests were sent by then, so ending the job
    /// here leaves the ECU unchanged. A cancel during the wait is [`JobError::Cancelled`]. The
    /// message carries no VIN (design 16.2).
    #[error("the per-vehicle lock could not be taken: {0}")]
    VehicleLock(#[source] GuardError),
    /// The job's guard slot was empty where the restart had to take the vehicle lock, which
    /// only a bug in this crate causes.
    #[error("the job's guards were missing when the per-vehicle lock was to be taken")]
    GuardsMissing,
}

/// What a job does with its journal.
enum JournalMode {
    /// A first run: the journal is created, and one that exists is an error.
    Create(JournalSetup),
    /// A job that ran before: its journal is opened and classified (`restart`).
    Resume(JournalSetup),
}

/// Where a job's guards stay during its run. The job thread keeps a handle, so the guards
/// outlive the thread even when the caller's future is dropped first; the caller takes them
/// back once the run ends.
type GuardSlot = Arc<Mutex<Option<JobGuards>>>;

/// The checks [`run_program`] makes before it touches the worker: the program's schema version and
/// size must suit this VM (`Vm::check_state`), its restart declaration must hold
/// (`Program::validate`), and the policy must allow every request in it at `permission`
/// (ADR-247). A caller can make them before it launches a worker for the job, with
/// [`policy::build_ceiling`] since the VCI is not known yet. Operands are not checked
/// exhaustively: a bad one, such as a missing constant, fails when the VM reaches it.
pub fn check_program(program: &Program, permission: Permission) -> Result<(), JobError> {
    // A program this VM cannot run must not reach the bus.
    Vm::new(program)
        .check_state(program)
        .map_err(|source| JobError::Vm { pc: 0, source })?;
    program.validate()?;
    refuse_beyond(program, permission)
}

/// The policy part of [`check_program`].
fn refuse_beyond(program: &Program, permission: Permission) -> Result<(), JobError> {
    policy::check_program(program, permission)
        .map_err(|(pc, source)| JobError::Refused { pc, source })
}

/// Opens a link with `config`, runs `program` on it to the end and closes the link. Returns
/// the final VM state; the procedure's results are on its stack.
///
/// Must be called on a multi-threaded runtime: the VM runs on a blocking thread that blocks on
/// the runtime for every primitive. That thread opens the link, runs the program and closes the
/// link, so no future holding worker resources is ever dropped halfway. Once open, the link is
/// closed however the job ends, also when the procedure panics; a failure to close it is
/// logged and does not change the results, but it marks the returned guards
/// ([`JobGuards::link_unconfirmed`], ADR-258): the worker may still hold the VCI, so the guards
/// keep their locks and a run refuses them with [`JobError::LinkUnconfirmed`] until
/// [`JobGuards::worker_gone`]. Dropping the returned future cancels the job:
/// the call or primitive in flight finishes, no further instruction runs, and the link is
/// closed. That happens after the future is gone, so a caller that dropped it must not hand
/// the worker to another job yet (ADR-235 consequences).
///
/// The program is checked against [`policy::build_ceiling`] before anything opens and against
/// the link's own permission once the VCI is known (ADR-247).
///
/// `guards` are the job's guards (`guards::JobGuards`, ADR-256, ADR-257, design 8.8.1). The
/// caller takes them before it calls: `JobGuards::take` for a program that writes
/// (`policy::writes`), `JobGuards::take_vci_only` for one that only reads. A writing program on
/// guards without the reprogramming slot ends in [`JobError::NoReprogrammingSlot`], checked
/// with the policy before anything opens. The run takes the guards by value, so two runs can
/// never use one set at once, holds them until the link is closed, and returns them with its
/// result. A dropped future releases them only once the job thread has ended.
pub async fn run_program(
    client: WorkerClient,
    config: &LinkConfig,
    program: Program,
    limits: JobLimits,
    guards: JobGuards,
) -> (Result<VmState, JobError>, JobGuards) {
    run_guarded(
        client,
        config,
        program,
        limits,
        policy::build_ceiling(),
        None,
        guards,
    )
    .await
}

/// [`run_program`] that also writes the write-job journal for a program with a flash recovery
/// plan (ADR-244, ADR-252): the journal of `journal.key` is created in `journal.dir` once the
/// link is open and the policy allows the program, before anything is sent to the ECU, and the
/// runner commits to it at the plan's boundaries (`journaling`). A commit that fails ends the
/// job before the request it guards is sent. A program without a plan keeps no journal and
/// creates no file. `guards` are taken and returned as for [`run_program`]; a program with a
/// plan writes, so they must hold the reprogramming slot (`JobGuards::take`).
pub async fn run_program_journaled(
    client: WorkerClient,
    config: &LinkConfig,
    program: Program,
    limits: JobLimits,
    journal: JournalSetup,
    guards: JobGuards,
) -> (Result<VmState, JobError>, JobGuards) {
    run_guarded(
        client,
        config,
        program,
        limits,
        policy::build_ceiling(),
        Some(JournalMode::Create(journal)),
        guards,
    )
    .await
}

/// Goes on with a job that ran before, from the journal [`run_program_journaled`] wrote under
/// `journal.key` (ADR-255). Once the link is open and the policy allows the program, and before
/// anything is sent to the ECU, the journal is opened and classified (`restart::classify`,
/// ADR-253):
/// - a plain start runs the program from its start on the same journal, its step count going on
///   after the journal's last record (`restart::next_steps`); a program without a flash
///   recovery plan and without a journal runs without one;
/// - an interrupted transfer makes the checks of `restart::check_before_ecu` (resume limit,
///   supply voltage), commits the incremented resume count and makes the gates of
///   `restart::check_gates` (ADR-229 item 2 step 2, ADR-261), which send only
///   ReadDataByIdentifier requests through the declared sources and read runtime inputs, nothing
///   that changes the ECU. Right after the ECU's VIN matched the job's, and before any further
///   ECU request, the guards are promoted to the per-vehicle lock (`JobGuards::take_vehicle`,
///   ADR-263): the job waits while another job holds that vehicle, with the guard slot not
///   locked, and a cancel then ends it in [`JobError::Cancelled`]. A lock that fails ends it in
///   [`JobError::VehicleLock`], still with nothing sent that changes the ECU. The lock stays in
///   the guards, which come back with the result (`JobGuards::holds_vehicle`); guards that
///   already hold the job's vehicle go on at once. A VIN that is not established or differs
///   takes no lock. Then `restart::teardown` ends the interrupted download (step 2b-1,
///   ADR-264): it sends one ECUReset when the gates allow it and the journal shows no
///   RequestTransferExit intent, and otherwise waits out the plan's session timeout plus
///   margin, sending nothing (a cancel during the wait is [`JobError::Cancelled`]); a journal
///   with the post-transfer steps complete gets neither. `restart::confirm_default_session`
///   then waits the ECU's startup time and reads F186 until the ECU reports its default session
///   (step 2b-2, ADR-265), with one passive teardown and one more attempt when the first fails
///   after a reset or on the completed path; an ECU that cannot be confirmed ends the job in
///   [`JobError::OnSiteInterventionRequired`] (`DefaultSessionNotConfirmed`). The rest of the
///   restart order (step 3 on) does not run in this agent, so a confirmed job ends in
///   [`JobError::OnSiteInterventionRequired`] (`RestartOrderUnavailable`, carrying the
///   teardown's outcome and the confirmation), or in [`JobError::IdentityMismatch`] when the
///   ECU's VIN is another vehicle's;
/// - a journal that rules a restart out, or a missing or unreadable one for a program with a
///   plan, ends the job in on-site intervention with nothing sent.
///
/// `guards` are taken and returned as for [`run_program`]. After an agent crash or a loss of the
/// device's power, the new run takes them with `JobGuards::take` before it calls this, waiting
/// while another job holds them, so a duplicate resume of the same job waits there without
/// opening a link. A job that survives a worker crash, a VCI disconnect or a loss of the
/// vehicle's supply alone passes the guards it got back to its next run. The journal's writer
/// lock is held while the job runs as well, so a second writer of the same journal ends in
/// `JobError::Journal(JournalError::InUse)` with nothing sent (ADR-255). The start deadline and
/// a server reservation are not taken here.
pub async fn resume_program_journaled(
    client: WorkerClient,
    config: &LinkConfig,
    program: Program,
    limits: JobLimits,
    journal: JournalSetup,
    guards: JobGuards,
) -> (Result<VmState, JobError>, JobGuards) {
    run_guarded(
        client,
        config,
        program,
        limits,
        policy::build_ceiling(),
        Some(JournalMode::Resume(journal)),
        guards,
    )
    .await
}

/// Puts `guards` in a slot, runs the job with it and takes the guards back, so every entry
/// point returns them with its result.
async fn run_guarded(
    client: WorkerClient,
    config: &LinkConfig,
    program: Program,
    limits: JobLimits,
    ceiling: Permission,
    journal: Option<JournalMode>,
    guards: JobGuards,
) -> (Result<VmState, JobError>, JobGuards) {
    let slot: GuardSlot = Arc::new(Mutex::new(Some(guards)));
    let result = run_program_within(
        client,
        config,
        program,
        limits,
        ceiling,
        journal,
        Arc::clone(&slot),
    )
    .await;
    let guards = slot
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .expect("nothing but this call takes the guards out of the slot");
    (result, guards)
}

/// [`run_program`] with the build's ceiling given, so tests can run under a lower one.
async fn run_program_within(
    client: WorkerClient,
    config: &LinkConfig,
    program: Program,
    limits: JobLimits,
    ceiling: Permission,
    journal: Option<JournalMode>,
    guards: GuardSlot,
) -> Result<VmState, JobError> {
    let handle = Handle::current();
    if handle.runtime_flavor() == RuntimeFlavor::CurrentThread {
        return Err(JobError::CurrentThreadRuntime);
    }
    // Guards whose last link was not confirmed closed may still have a worker on the VCI
    // (ADR-258): nothing opens on them.
    if guards
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(JobGuards::link_unconfirmed)
    {
        return Err(JobError::LinkUnconfirmed);
    }
    check_program(&program, ceiling)?;
    // A program that writes needs the device's reprogramming slot (design 8.8.1, ADR-257).
    let holds_slot = guards
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(JobGuards::holds_slot);
    if policy::writes(&program) && !holds_slot {
        return Err(JobError::NoReprogrammingSlot);
    }
    let config = config.clone();

    let cancel = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let cancelled = Arc::clone(&cancel.0);
    tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            run_job(
                handle, client, &config, &program, limits, &cancelled, journal, guards,
            )
        }))
        .unwrap_or(Err(JobError::Panicked))
    })
    .await
    .unwrap_or_else(|error| {
        Err(if error.is_cancelled() {
            // The runtime is shutting down.
            JobError::Cancelled
        } else {
            JobError::Panicked
        })
    })
}

/// The whole job, on the blocking thread: open, run, close.
#[expect(
    clippy::too_many_arguments,
    reason = "the job's whole input, split only by what the caller owns"
)]
fn run_job(
    handle: Handle,
    mut client: WorkerClient,
    config: &LinkConfig,
    program: &Program,
    limits: JobLimits,
    cancelled: &AtomicBool,
    journal: Option<JournalMode>,
    guards: GuardSlot,
) -> Result<VmState, JobError> {
    // The job's guards stay held until the end of this function, after the link is closed
    // (`OpenLink`, declared below, closes it on every way out, a panic included): declared
    // first, this handle is dropped last, so even a caller whose future is gone cannot hand the
    // VCI to another job while this one still tears its link down.
    let _guards = Arc::clone(&guards);
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    let timings = Timings::for_link(config);
    // An open that fails partway closes what it opened; when even that fails, or the open
    // panics, the worker may still hold the module or the link (ADR-258).
    let opened = std::panic::catch_unwind(AssertUnwindSafe(|| {
        handle.block_on(link::open_tracked(&mut client, config, timings.unary))
    }));
    let link = match opened {
        Ok(Ok(link)) => link,
        Ok(Err(failure)) => {
            if !failure.cleaned_up {
                mark_link_unconfirmed(&guards);
            }
            return Err(JobError::Link(failure.error));
        }
        Err(_) => {
            mark_link_unconfirmed(&guards);
            return Err(JobError::Panicked);
        }
    };
    let permission = link.permission;
    // Declared before the host, which owns the link's event stream: on a panic the stream is
    // dropped first, then the link is closed.
    let open_link = OpenLink {
        handle: handle.clone(),
        client: client.clone(),
        module_handle: link.module_handle,
        cll_handle: link.cll_handle,
        deadline: timings.unary,
        guards: Arc::clone(&guards),
        closed: false,
    };
    let mut host = WorkerHost::new(handle.clone(), client, link, timings);
    // The VCI is known now. Nothing has been sent to the ECU yet, so a program that needs more
    // than this link allows is refused with no effect on it.
    if let Err(error) = refuse_beyond(program, permission) {
        drop(host);
        open_link.close();
        return Err(error);
    }
    // Still before anything is sent: a job that cannot keep its journal sends nothing. A link
    // that fails to open, or a program the link refuses, leaves no journal behind.
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| match journal {
        Some(JournalMode::Create(setup)) if !program.flash.is_empty() => {
            let mut journal = JobJournal::create(setup)?;
            run_on(program, &mut host, limits, cancelled, Some(&mut journal))
        }
        Some(JournalMode::Resume(setup)) => resume_on(
            program,
            &mut host,
            limits,
            cancelled,
            Journal::open(&setup.dir, &setup.key),
            setup.sources,
            setup.vin.as_ref(),
            &guards,
        ),
        _ => run_on(
            program,
            &mut host,
            limits,
            cancelled,
            None::<&mut JobJournal>,
        ),
    }))
    .unwrap_or(Err(JobError::Panicked));
    // The event stream goes before the link is closed.
    drop(host);
    open_link.close();
    result
}

/// A link that is open on the worker. It is closed exactly once, by [`OpenLink::close`] or, when
/// the job panics, by `Drop`. The job's result stands whether or not that works; a failure is
/// logged and marks the job's guards (`JobGuards::mark_link_unconfirmed`, ADR-258), since the
/// worker may then still hold the VCI.
struct OpenLink {
    handle: Handle,
    client: WorkerClient,
    module_handle: vci_service_interface::ModuleHandle,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
    deadline: Duration,
    guards: GuardSlot,
    closed: bool,
}

impl OpenLink {
    /// Closes the link. The link's event stream must be dropped before.
    fn close(mut self) {
        self.teardown();
    }

    fn teardown(&mut self) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        // Closing on a runtime that is shutting down can panic.
        let closed = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.handle.block_on(link::teardown(
                &mut self.client,
                self.module_handle,
                self.cll_handle,
                self.deadline,
            ))
        }));
        match closed {
            Ok(Ok(())) => return,
            Ok(Err(error)) => tracing::warn!(%error, "could not close the link"),
            Err(_) => tracing::warn!("closing the link panicked"),
        }
        mark_link_unconfirmed(&self.guards);
    }
}

/// Marks the guards in `slot`: the worker may still hold the job's link (ADR-258).
fn mark_link_unconfirmed(slot: &GuardSlot) {
    if let Some(guards) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        guards.mark_link_unconfirmed();
    }
}

impl Drop for OpenLink {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Sets the flag when the job's future is dropped.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Goes on with a job from its journal as `Journal::open` gave it (ADR-255; see
/// [`resume_program_journaled`]). Nothing is sent to the ECU before the classification and the
/// checks of `restart::check_before_ecu` passed; then `restart::check_gates` sends only
/// ReadDataByIdentifier requests (ADR-229 item 2 step 2), `restart::teardown` sends at most
/// one ECUReset (step 2b-1, ADR-264), and `restart::confirm_default_session` reads F186
/// (step 2b-2, ADR-265). `vin` is the job's target VIN. Once
/// the ECU's VIN matched it, the gates promote `guards` to the per-vehicle lock (ADR-263; see
/// [`promote_to_vehicle`]).
#[expect(
    clippy::too_many_arguments,
    reason = "the job's whole input, split only by what the caller owns"
)]
fn resume_on<H, S>(
    program: &Program,
    host: &mut H,
    limits: JobLimits,
    cancelled: &AtomicBool,
    opened: Result<Journal<S>, JournalError>,
    sources: crate::inputs::ServiceSources,
    vin: Option<&crate::journal::Vin>,
    guards: &GuardSlot,
) -> Result<VmState, JobError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs + TransferProgress,
    S: Store,
{
    // Another run of this job holds the journal: it, not this one, goes on with the job.
    if let Err(JournalError::InUse) = opened {
        return Err(JobError::Journal(JournalError::InUse));
    }
    // A resume names the VIN the job's first run recorded, or the job's own data changed
    // (ADR-261): nothing is sent and no resume is counted.
    if let Ok(journal) = &opened
        && journal.state().facts.target_vin.as_ref() != vin
    {
        return Err(JobError::OnSiteInterventionRequired(
            OnSiteReason::TargetVinDiffers,
        ));
    }
    match restart::classify(program, opened.as_ref().map(Journal::state)) {
        RestartDecision::OnSiteInterventionRequired(reason) => {
            Err(JobError::OnSiteInterventionRequired(reason))
        }
        RestartDecision::PlainStart => match opened {
            Ok(journal) if !program.flash.is_empty() => {
                let first_step = restart::next_steps(journal.state());
                let mut journal = JobJournal::new(journal, sources);
                run_on_from(
                    program,
                    host,
                    limits,
                    cancelled,
                    Some(&mut journal),
                    first_step,
                )
            }
            // A program without a plan keeps no journal (ADR-252 item 7).
            _ => run_on(program, host, limits, cancelled, None::<&mut JobJournal<S>>),
        },
        RestartDecision::Restart(point) => {
            // `classify` restarts only from a journal it read.
            let mut journal = opened?;
            restart::check_before_ecu(program, &point, &mut journal, host, cancelled)?;
            let poll = limits.wait_poll.max(Duration::from_millis(1));
            let gate = restart::check_gates(program, &point, &sources, host, cancelled, |vin| {
                promote_to_vehicle(guards, vin, poll, cancelled)
            })?;
            // `classify` took the point from one of the program's plans.
            let plan = program
                .flash
                .iter()
                .find(|plan| plan.flash_session == point.flash_session)
                .ok_or(JobError::Journal(JournalError::Invariant(
                    "the restart point names no plan of the program",
                )))?;
            let teardown = restart::teardown(gate, &point, &plan.timing, host, poll, cancelled)?;
            let confirmed = restart::confirm_default_session(
                point.flash_session,
                teardown.clone(),
                &plan.timing,
                host,
                poll,
                cancelled,
            )?;
            Err(JobError::OnSiteInterventionRequired(
                OnSiteReason::RestartOrderUnavailable {
                    flash_session: point.flash_session,
                    teardown,
                    confirmed,
                },
            ))
        }
    }
}

/// Takes the job's guards out of `slot`, takes the per-vehicle lock of `vin` on them, waiting
/// while another job holds it (`JobGuards::take_vehicle`, ADR-262), and puts them back. The
/// slot's mutex is held only to take them out and to put them back, never across the wait. A
/// panic or an early return puts them back as well ([`SlotReturn`]), since the caller expects
/// them in the slot at the end. A cancel is [`JobError::Cancelled`]; any other failure is
/// [`JobError::VehicleLock`], and an empty slot [`JobError::GuardsMissing`].
fn promote_to_vehicle(
    slot: &GuardSlot,
    vin: &crate::journal::Vin,
    poll: Duration,
    cancelled: &AtomicBool,
) -> Result<(), JobError> {
    let taken = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut owned = SlotReturn {
        slot,
        guards: Some(taken.ok_or(JobError::GuardsMissing)?),
    };
    let guards = owned.guards.as_mut().expect("set just above");
    guards
        .take_vehicle(vin, poll, cancelled)
        .map_err(|error| match error {
            GuardError::Cancelled => JobError::Cancelled,
            other => JobError::VehicleLock(other),
        })
}

/// Guards taken out of a slot, which go back into it when this is dropped.
struct SlotReturn<'a> {
    slot: &'a GuardSlot,
    guards: Option<JobGuards>,
}

impl Drop for SlotReturn<'_> {
    fn drop(&mut self) {
        if let Some(guards) = self.guards.take() {
            *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(guards);
        }
    }
}

/// The step loop, on the blocking thread. With a journal, it commits the plan's markers each
/// time execution arrives at an instruction, before that instruction runs, and what a completed
/// instruction calls for right after it (`journaling`).
fn run_on<H, S>(
    program: &Program,
    host: &mut H,
    limits: JobLimits,
    cancelled: &AtomicBool,
    journal: Option<&mut JobJournal<S>>,
) -> Result<VmState, JobError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs + TransferProgress,
    S: Store,
{
    run_on_from(program, host, limits, cancelled, journal, 0)
}

/// [`run_on`] from the program's start with the step count at `first_step`, so a run on an
/// existing journal records its steps after the journal's last one.
fn run_on_from<H, S>(
    program: &Program,
    host: &mut H,
    limits: JobLimits,
    cancelled: &AtomicBool,
    mut journal: Option<&mut JobJournal<S>>,
    first_step: u64,
) -> Result<VmState, JobError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs + TransferProgress,
    S: Store,
{
    let wait_poll = limits.wait_poll.max(Duration::from_millis(1));
    let mut vm = Vm::new(program);
    vm.state.steps = first_step;
    // Whether the journal has seen this arrival yet (a timer wait polls the same instruction
    // again).
    let mut arrived = true;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(JobError::Cancelled);
        }
        if vm.state.steps >= limits.max_steps {
            return Err(JobError::StepLimit(vm.state.steps));
        }
        if std::mem::take(&mut arrived)
            && let Some(journal) = journal.as_deref_mut()
        {
            // An intent marker is only journaled for a request the next step sends: the VM's
            // own checks run first (ADR-233 item 3), so an instruction it refuses fails here,
            // with no marker before it.
            vm.current_op(program).map_err(|source| JobError::Vm {
                pc: vm.state.pc,
                source,
            })?;
            journal.arrive(program, &vm.state, host, cancelled)?;
            // The identity reads at a boundary take time; a cancel during them stops the job
            // before the boundary's instruction runs.
            if cancelled.load(Ordering::Relaxed) {
                return Err(JobError::Cancelled);
            }
        }
        let pc = vm.state.pc;
        let at = StepRef {
            pc,
            steps: vm.state.steps,
        };
        let outcome = vm.step(program, host);
        if vm.state.steps > at.steps {
            // The instruction completed.
            if let Some(journal) = journal.as_deref_mut() {
                journal.completed(program, at, &vm.state, host)?;
            }
            arrived = true;
        }
        match outcome {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => return Ok(vm.state),
            Ok(StepOutcome::Waiting(WaitingOn::Timer)) => std::thread::sleep(wait_poll),
            Ok(StepOutcome::Waiting(on)) => return Err(JobError::Unanswerable(on)),
            Err(StepError::Vm(source)) => return Err(JobError::Vm { pc, source }),
            Err(StepError::Host(source)) => return Err(JobError::Host { pc, source }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use diag_ir::{IR_SCHEMA_VERSION, Op, Value};
    use vci_service_interface::{ComLogicalLinkHandle, ModuleHandle};

    use super::*;

    /// Answers every service request with its SID plus 0x40, and counts the calls.
    #[derive(Default)]
    struct FakeHost {
        requests: u32,
        waits: u32,
        /// Set after this many requests, as a dropped job future would.
        cancel_after: Option<(u32, Arc<AtomicBool>)>,
    }

    impl DiagHost for FakeHost {
        type Error = HostError;

        fn service_request(&mut self, service: u16, _payload: &[u8]) -> Result<Vec<u8>, HostError> {
            self.requests += 1;
            if let Some((after, flag)) = &self.cancel_after
                && self.requests >= *after
            {
                flag.store(true, Ordering::Relaxed);
            }
            Ok(vec![service as u8 + 0x40])
        }
        fn read_dtc(&mut self, _mask: u8) -> Result<Vec<u8>, HostError> {
            Err(HostError::NoResponse)
        }
        fn routine_control(&mut self, _: u16, _: u8, _: &[u8]) -> Result<Vec<u8>, HostError> {
            Err(HostError::NoResponse)
        }
        fn security_access(
            &mut self,
            _: u64,
            _: u8,
            _: &[u8],
        ) -> Result<Option<Vec<u8>>, HostError> {
            Err(HostError::Unsupported("SecurityAccess"))
        }
        fn flash_transfer(&mut self, _: u32, _: &[u8]) -> Result<(), HostError> {
            Err(HostError::Unsupported("FlashTransfer"))
        }
        fn wait(&mut self, _: u64, _: u32) -> Result<bool, HostError> {
            self.waits += 1;
            Ok(self.waits > 2)
        }
        fn hmi_request(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, HostError> {
            Ok(None)
        }
        fn record_input(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, HostError> {
            Err(HostError::Unsupported("RecordInput"))
        }
        fn monitor_capture(&mut self, _: u32) -> Result<(), HostError> {
            Err(HostError::Unsupported("MonitorCapture"))
        }
        fn log(&mut self, _: u8, _: &str) {}
    }

    impl RuntimeInputs for FakeHost {
        fn read(&mut self, _: diag_ir::RuntimeInput) -> Result<crate::inputs::Reading, HostError> {
            Ok(crate::inputs::Reading::CannotBeEstablished)
        }
    }

    impl TransferProgress for FakeHost {
        fn transfer_block_index(&self) -> Option<u64> {
            None
        }
    }

    /// `run_on` without a journal.
    fn run_plain(
        program: &Program,
        host: &mut FakeHost,
        limits: JobLimits,
        cancelled: &AtomicBool,
    ) -> Result<VmState, JobError> {
        run_on(program, host, limits, cancelled, None::<&mut JobJournal>)
    }

    fn program(code: Vec<Op>) -> Program {
        Program {
            schema_version: IR_SCHEMA_VERSION,
            code,
            constants: vec![vec![0x01]],
            sections: Vec::new(),
            source_map: Vec::new(),
            identity: Default::default(),
            preconditions: Default::default(),
            flash: Vec::new(),
        }
    }

    fn run(code: Vec<Op>, max_steps: u64, host: &mut FakeHost) -> Result<VmState, JobError> {
        let limits = JobLimits {
            max_steps,
            ..JobLimits::default()
        };
        run_plain(&program(code), host, limits, &AtomicBool::new(false))
    }

    fn two_requests() -> Vec<Op> {
        vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x10 },
        ]
    }

    #[test]
    fn a_program_with_an_invalid_declaration_is_refused() {
        let mut bad = program(two_requests());
        bad.flash.push(diag_ir::FlashRecovery {
            flash_session: 1,
            stage: 1,
            max_resumes: 1,
            recovery_required: diag_ir::RecoveryRequired::Never,
            boundaries: diag_ir::RecoveryBoundaries {
                entry_pc: 0,
                erase_pc: 3,
                transfer_exit_pc: 2,
                post_transfer_end_pc: 4,
            },
            timing: diag_ir::RecoveryTiming {
                session_timeout_millis: 5000,
                teardown_margin_millis: 0,
                ecu_startup_millis: 0,
                confirmation_window_millis: 0,
            },
            version_read_retries: 0,
            no_application: None,
        });
        assert!(matches!(
            check_program(&bad, Permission::ReadOnly),
            Err(JobError::Program(
                diag_ir::ProgramError::BoundaryOutOfOrder { flash_session: 1 }
            ))
        ));
        assert!(check_program(&program(Vec::new()), Permission::ReadOnly).is_ok());
    }

    #[test]
    fn a_restartable_plan_with_an_unmapped_precondition_is_refused() {
        let source = diag_ir::Source::EcuService {
            service_id: 1,
            field_id: 1,
        };
        let mut bad = program(vec![
            Op::Pop,
            Op::RoutineControl { routine: 1, sub: 1 },
            Op::ServiceRequest { service: 0x34 },
            Op::ServiceRequest { service: 0x37 },
            Op::Pop,
        ]);
        bad.identity = diag_ir::IdentitySources {
            vin: Some(source),
            hardware_part_number: Some(source),
            software_version: Some(source),
        };
        bad.preconditions.engine = Some(diag_ir::Precondition {
            satisfied: diag_ir::Satisfied { lower: 0, upper: 0 },
            default_session: Some(source),
            programming_session: None,
        });
        bad.flash.push(diag_ir::FlashRecovery {
            flash_session: 1,
            stage: 1,
            max_resumes: 1,
            recovery_required: diag_ir::RecoveryRequired::Never,
            boundaries: diag_ir::RecoveryBoundaries {
                entry_pc: 0,
                erase_pc: 1,
                transfer_exit_pc: 3,
                post_transfer_end_pc: 5,
            },
            timing: diag_ir::RecoveryTiming {
                session_timeout_millis: 5000,
                teardown_margin_millis: 0,
                ecu_startup_millis: 0,
                confirmation_window_millis: 0,
            },
            version_read_retries: 0,
            no_application: None,
        });
        assert!(matches!(
            check_program(&bad, Permission::ReadOnly),
            Err(JobError::Program(
                diag_ir::ProgramError::UnmappedPrecondition {
                    kind: diag_ir::PreconditionKind::Engine,
                    session: diag_ir::SessionKind::Programming,
                }
            ))
        ));
    }

    #[test]
    fn a_program_runs_to_the_end() {
        let mut host = FakeHost::default();
        let state = run(two_requests(), 4, &mut host).unwrap();
        assert_eq!(
            state.stack,
            [Value::Bytes(vec![0x62]), Value::Bytes(vec![0x50])]
        );
        assert_eq!(host.requests, 2);
    }

    #[test]
    fn the_step_limit_stops_a_longer_program() {
        let mut host = FakeHost::default();
        assert!(matches!(
            run(two_requests(), 3, &mut host),
            Err(JobError::StepLimit(3))
        ));
        assert_eq!(host.requests, 1);
    }

    #[test]
    fn a_timer_wait_is_polled_until_it_ends() {
        let mut host = FakeHost::default();
        let limits = JobLimits {
            wait_poll: Duration::ZERO,
            ..JobLimits::default()
        };
        let program = program(vec![Op::Wait { millis: 5 }]);
        run_plain(&program, &mut host, limits, &AtomicBool::new(false)).unwrap();
        assert_eq!(host.waits, 3);
    }

    #[test]
    fn a_wait_the_agent_cannot_answer_ends_the_job() {
        let mut host = FakeHost::default();
        assert!(matches!(
            run(vec![Op::HmiRequest { form: 0 }], 10, &mut host),
            Err(JobError::Unanswerable(WaitingOn::Hmi))
        ));
    }

    #[test]
    fn host_and_vm_errors_carry_the_pc() {
        let mut host = FakeHost::default();
        assert!(matches!(
            run(vec![Op::PushI64(1), Op::ReadDtc { mask: 8 }], 10, &mut host),
            Err(JobError::Host {
                pc: 1,
                source: HostError::NoResponse
            })
        ));
        assert!(matches!(
            run(
                vec![Op::PushI64(1), Op::ServiceRequest { service: 0x22 }],
                10,
                &mut host
            ),
            Err(JobError::Vm { pc: 1, .. })
        ));
    }

    #[test]
    fn a_cancelled_job_stops_before_the_next_instruction() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut host = FakeHost {
            cancel_after: Some((1, Arc::clone(&flag))),
            ..FakeHost::default()
        };
        assert!(matches!(
            run_plain(
                &program(two_requests()),
                &mut host,
                JobLimits::default(),
                &flag
            ),
            Err(JobError::Cancelled)
        ));
        assert_eq!(host.requests, 1);
    }

    // ------------------------------------------------------------ journaling (ADR-252)

    /// Counts the journal's commits in a cell the host reads, and fails the commit numbered
    /// `fail_at` (1-based).
    struct CountingStore {
        commits: Rc<Cell<u64>>,
        fail_at: Option<u64>,
    }

    impl Store for CountingStore {
        fn append_sync(&mut self, _: &[u8]) -> std::io::Result<()> {
            if self.fail_at == Some(self.commits.get() + 1) {
                return Err(std::io::Error::other("disk full"));
            }
            self.commits.set(self.commits.get() + 1);
            Ok(())
        }
    }

    /// What a [`FlashHost`] was asked to do.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Sent {
        Service(u16, Vec<u8>),
        Routine(u16),
        Block,
    }

    /// How a [`FlashHost`] answers an ECUReset.
    #[derive(Debug, Clone, Copy)]
    enum ResetAnswer {
        Positive,
        /// A negative response with this code.
        Refuse(u8),
        /// No answer.
        NoAnswer,
        /// A positive-looking answer that does not echo the sub-function.
        Garbled,
        /// A positive response with a byte after the sub-function, which a hard reset's has not.
        Trailing,
    }

    /// How a [`FlashHost`] answers a read of F186 (the active session).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SessionAnswer {
        /// The default session.
        Default,
        /// This session value (for example 0x03, the extended session).
        Other(u8),
        /// A negative response with this code.
        Refuse(u8),
        /// No answer.
        NoAnswer,
    }

    /// An ECU that downloads: it answers ReadDataByIdentifier F191 and F195, accepts every
    /// other request and every block, and counts the blocks since the last RequestDownload as
    /// the worker's host does. It logs each request with the journal commits made before it.
    struct FlashHost {
        commits: Rc<Cell<u64>>,
        log: Vec<(Sent, u64)>,
        transfer: Option<u64>,
        /// The answer to F195; `None` answers it negatively.
        software_version: Option<Vec<u8>>,
        /// Set when a ReadDataByIdentifier for this identifier arrives, as a dropped job
        /// future would.
        cancel_on_read: Option<([u8; 2], Arc<AtomicBool>)>,
        /// A routine whose response is lost.
        lose_routine: Option<u16>,
        /// The supply voltage the VCI reports; `None` reports none.
        voltage: Option<i64>,
        voltage_reads: u32,
        /// Reading the voltage fails, as a worker that cannot be reached would.
        voltage_fails: bool,
        /// Set while the voltage is read, as a dropped job future would.
        cancel_on_voltage: Option<Arc<AtomicBool>>,
        /// The answer to F190 (the VIN); `None` answers it negatively.
        vin: Option<Vec<u8>>,
        /// Identifiers whose ReadDataByIdentifier fails, as a worker that cannot be reached
        /// would.
        fail_reads: Vec<[u8; 2]>,
        /// The answer to F191 (the hardware part number); `None` answers it negatively.
        hardware: Option<Vec<u8>>,
        /// The readings of the runtime inputs other than the supply voltage, which `voltage`
        /// answers; an input not set is `CannotBeEstablished`.
        inputs: crate::inputs::FixedInputs,
        /// Every request but the reads of F186 is also pushed here (as in `sent`), for a test
        /// that looks at the host while the job runs on another thread.
        mirror: Option<Arc<Mutex<Vec<Sent>>>>,
        /// The answer to ECUReset.
        reset: ResetAnswer,
        /// A service whose response is lost.
        lose_service: Option<u16>,
        /// The answers to the first reads of F186, one per read; later reads get `session`.
        session_script: std::collections::VecDeque<SessionAnswer>,
        /// The answer to F186 when the script is used up.
        session: SessionAnswer,
        /// When each read of F186 arrived.
        session_reads: Vec<std::time::Instant>,
    }

    impl FlashHost {
        fn new(commits: Rc<Cell<u64>>) -> Self {
            Self {
                commits,
                log: Vec::new(),
                transfer: None,
                software_version: Some(b"SW01".to_vec()),
                cancel_on_read: None,
                lose_routine: None,
                voltage: None,
                voltage_reads: 0,
                voltage_fails: false,
                cancel_on_voltage: None,
                fail_reads: Vec::new(),
                vin: Some(TARGET_VIN.as_bytes().to_vec()),
                hardware: Some(b"HW01".to_vec()),
                inputs: crate::inputs::FixedInputs::new(),
                mirror: None,
                reset: ResetAnswer::Positive,
                lose_service: None,
                session_script: std::collections::VecDeque::new(),
                session: SessionAnswer::Default,
                session_reads: Vec::new(),
            }
        }

        /// What was sent, without the reads of F186 (the restart's default-session
        /// confirmation, which `session_reads` counts).
        fn sent(&self) -> Vec<Sent> {
            self.log
                .iter()
                .map(|(sent, _)| sent.clone())
                .filter(|sent| *sent != read_of([0xF1, 0x86]))
                .collect()
        }
    }

    impl RuntimeInputs for FlashHost {
        fn read(
            &mut self,
            input: diag_ir::RuntimeInput,
        ) -> Result<crate::inputs::Reading, HostError> {
            if input == diag_ir::RuntimeInput::SupplyVoltageMillivolts
                && let Some(flag) = &self.cancel_on_voltage
            {
                flag.store(true, Ordering::Relaxed);
            }
            if input == diag_ir::RuntimeInput::SupplyVoltageMillivolts && self.voltage_fails {
                self.voltage_reads += 1;
                return Err(HostError::NoResponse);
            }
            Ok(match (input, self.voltage) {
                (diag_ir::RuntimeInput::SupplyVoltageMillivolts, Some(millivolts)) => {
                    self.voltage_reads += 1;
                    crate::inputs::Reading::Value(millivolts)
                }
                (diag_ir::RuntimeInput::SupplyVoltageMillivolts, None) => {
                    self.voltage_reads += 1;
                    crate::inputs::Reading::CannotBeEstablished
                }
                _ => return self.inputs.read(input),
            })
        }
    }

    impl TransferProgress for FlashHost {
        fn transfer_block_index(&self) -> Option<u64> {
            self.transfer
        }
    }

    impl DiagHost for FlashHost {
        type Error = HostError;

        fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, HostError> {
            self.log
                .push((Sent::Service(service, payload.to_vec()), self.commits.get()));
            if let Some(mirror) = &self.mirror
                && !(service == 0x22 && payload == [0xF1, 0x86])
            {
                mirror
                    .lock()
                    .unwrap()
                    .push(Sent::Service(service, payload.to_vec()));
            }
            if let Some((did, flag)) = &self.cancel_on_read
                && service == 0x22
                && payload == did
            {
                flag.store(true, Ordering::Relaxed);
            }
            if service == 0x22 && self.fail_reads.iter().any(|did| payload == did) {
                return Err(HostError::NoResponse);
            }
            if self.lose_service == Some(service) {
                return Err(HostError::NoResponse);
            }
            match (service, payload) {
                (0x11, _) => match self.reset {
                    ResetAnswer::Positive => Ok(vec![0x51, payload[0]]),
                    ResetAnswer::Refuse(nrc) => Ok(vec![0x7F, 0x11, nrc]),
                    ResetAnswer::NoAnswer => Err(HostError::NoResponse),
                    ResetAnswer::Garbled => Ok(vec![0x51]),
                    ResetAnswer::Trailing => Ok(vec![0x51, 0x01, 0x00]),
                },
                (0x22, [0xF1, 0x90]) => Ok(match &self.vin {
                    Some(vin) => [&[0x62, 0xF1, 0x90][..], vin].concat(),
                    None => vec![0x7F, 0x22, 0x31],
                }),
                (0x22, [0xF1, 0x91]) => Ok(match &self.hardware {
                    Some(hardware) => [&[0x62, 0xF1, 0x91][..], hardware].concat(),
                    None => vec![0x7F, 0x22, 0x31],
                }),
                (0x22, [0xF1, 0x86]) => {
                    self.session_reads.push(std::time::Instant::now());
                    match self.session_script.pop_front().unwrap_or(self.session) {
                        SessionAnswer::Default => Ok(vec![0x62, 0xF1, 0x86, 0x01]),
                        SessionAnswer::Other(value) => Ok(vec![0x62, 0xF1, 0x86, value]),
                        SessionAnswer::Refuse(nrc) => Ok(vec![0x7F, 0x22, nrc]),
                        SessionAnswer::NoAnswer => Err(HostError::NoResponse),
                    }
                }
                (0x22, [0xF1, 0x95]) => Ok(match &self.software_version {
                    Some(version) => [&[0x62, 0xF1, 0x95][..], version].concat(),
                    None => vec![0x7F, 0x22, 0x31],
                }),
                (0x34, _) => {
                    self.transfer = Some(0);
                    Ok(vec![0x74])
                }
                (0x37, _) => {
                    self.transfer = None;
                    Ok(vec![0x77])
                }
                _ => Ok(vec![service as u8 + 0x40]),
            }
        }
        fn read_dtc(&mut self, _: u8) -> Result<Vec<u8>, HostError> {
            Err(HostError::NoResponse)
        }
        fn routine_control(&mut self, routine: u16, _: u8, _: &[u8]) -> Result<Vec<u8>, HostError> {
            self.log.push((Sent::Routine(routine), self.commits.get()));
            if self.lose_routine == Some(routine) {
                return Err(HostError::NoResponse);
            }
            Ok(vec![0x71])
        }
        fn security_access(
            &mut self,
            _: u64,
            _: u8,
            _: &[u8],
        ) -> Result<Option<Vec<u8>>, HostError> {
            Err(HostError::Unsupported("SecurityAccess"))
        }
        fn flash_transfer(&mut self, _: u32, _: &[u8]) -> Result<(), HostError> {
            self.log.push((Sent::Block, self.commits.get()));
            let count = self.transfer.as_mut().ok_or(HostError::NoTransferActive)?;
            *count += 1;
            Ok(())
        }
        fn wait(&mut self, _: u64, _: u32) -> Result<bool, HostError> {
            Ok(true)
        }
        fn hmi_request(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, HostError> {
            Ok(None)
        }
        fn record_input(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, HostError> {
            Err(HostError::Unsupported("RecordInput"))
        }
        fn monitor_capture(&mut self, _: u32) -> Result<(), HostError> {
            Err(HostError::Unsupported("MonitorCapture"))
        }
        fn log(&mut self, _: u8, _: &str) {}
    }

    const ENTRY: u32 = 3;
    const ERASE: u32 = 4;
    const EXIT: u32 = 16;
    const SESSION_TIMEOUT_MS: u32 = 20;
    const TEARDOWN_MARGIN_MS: u32 = 10;

    /// A programming session, then a plan: entry at 3, a routine-control erase at 4, a
    /// RequestDownload, three blocks, RequestTransferExit at 16, and the end of the code as the
    /// post-transfer end.
    fn flash_program() -> Program {
        let mut code = vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x10 },
            Op::Pop,
            Op::PushBytes(0),
            Op::RoutineControl {
                routine: 0xFF00,
                sub: 1,
            },
            Op::Pop,
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x34 },
            Op::Pop,
        ];
        for _ in 0..3 {
            code.extend([Op::PushBytes(0), Op::FlashTransfer { block: 1 }]);
        }
        code.extend([
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x37 },
            Op::Pop,
        ]);
        let mut program = program(code);
        let ecu = |field_id| diag_ir::Source::EcuService {
            service_id: 1,
            field_id,
        };
        program.identity = diag_ir::IdentitySources {
            vin: Some(ecu(3)),
            hardware_part_number: Some(ecu(1)),
            software_version: Some(ecu(2)),
        };
        program.flash.push(diag_ir::FlashRecovery {
            flash_session: 1,
            stage: 7,
            max_resumes: 1,
            recovery_required: diag_ir::RecoveryRequired::Never,
            boundaries: diag_ir::RecoveryBoundaries {
                entry_pc: ENTRY,
                erase_pc: ERASE,
                transfer_exit_pc: EXIT,
                post_transfer_end_pc: program.code.len() as u32,
            },
            // Short, since the passive teardown of a restart waits them out.
            timing: diag_ir::RecoveryTiming {
                session_timeout_millis: SESSION_TIMEOUT_MS,
                teardown_margin_millis: TEARDOWN_MARGIN_MS,
                ecu_startup_millis: 0,
                confirmation_window_millis: 0,
            },
            version_read_retries: 0,
            no_application: None,
        });
        program.validate().expect("the fixture is a valid program");
        program
    }

    /// The VIN the jobs of the fixture target, and the one the fixture ECU answers by default.
    const TARGET_VIN: &str = "WDB12345678901234";

    fn identity_sources() -> crate::inputs::ServiceSources {
        let field = |field_id, did: u8, length| crate::inputs::ServiceField {
            service_id: 1,
            field_id,
            request: vec![0x22, 0xF1, did],
            offset: 2,
            length,
            encoding: crate::inputs::Encoding::Ascii,
        };
        crate::inputs::ServiceSources::new(vec![
            field(1, 0x91, 4),
            field(2, 0x95, 4),
            field(3, 0x90, 17),
        ])
        .unwrap()
    }

    fn job_key() -> crate::journal::JobKey {
        crate::journal::JobKey {
            job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e6f".to_owned()),
            generation: 1,
        }
    }

    fn counting_journal(
        commits: &Rc<Cell<u64>>,
        fail_at: Option<u64>,
    ) -> JobJournal<CountingStore> {
        let store = CountingStore {
            commits: Rc::clone(commits),
            fail_at,
        };
        JobJournal::new(
            crate::journal::Journal::on_store(store, job_key()),
            identity_sources(),
        )
    }

    fn run_flash<S: Store>(
        host: &mut FlashHost,
        journal: &mut JobJournal<S>,
    ) -> Result<VmState, JobError> {
        run_on(
            &flash_program(),
            host,
            JobLimits::default(),
            &AtomicBool::new(false),
            Some(journal),
        )
    }

    /// Each marker is committed before the request it guards, the identity before the erase,
    /// and each block after the ECU confirmed it.
    #[test]
    fn the_journal_commits_come_before_the_requests_they_guard() {
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        let mut journal = counting_journal(&commits, None);
        run_flash(&mut host, &mut journal).unwrap();
        // Commits: 1 the step into the entry with the VM state, 2 the hardware part number,
        // 3 the software version, 4 the transfer start, 5 the erase's step, 6 the
        // RequestDownload's step, 7-12 each block and its step, 13 the exit marker, 14
        // RequestTransferExit's step, 15 the completion.
        assert_eq!(
            host.log,
            [
                (Sent::Service(0x10, vec![0x01]), 0),
                (Sent::Service(0x22, vec![0xF1, 0x91]), 1),
                (Sent::Service(0x22, vec![0xF1, 0x95]), 2),
                (Sent::Routine(0xFF00), 4),
                (Sent::Service(0x34, vec![0x01]), 5),
                (Sent::Block, 6),
                (Sent::Block, 8),
                (Sent::Block, 10),
                (Sent::Service(0x37, vec![0x01]), 13),
            ]
        );
        assert_eq!(commits.get(), 15);
        let facts = &journal.journal().state().facts;
        let transfer = facts.transfer.as_ref().unwrap();
        assert_eq!(transfer.last_block, Some(3));
        assert!(transfer.exit.as_ref().unwrap().complete);
    }

    #[test]
    fn a_failed_transfer_start_commit_ends_the_job_before_the_erase() {
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        let mut journal = counting_journal(&commits, Some(4));
        let result = run_flash(&mut host, &mut journal);
        assert!(
            matches!(result, Err(JobError::Journal(JournalError::Io(_)))),
            "{result:?}"
        );
        assert_eq!(
            host.sent(),
            [
                Sent::Service(0x10, vec![0x01]),
                Sent::Service(0x22, vec![0xF1, 0x91]),
                Sent::Service(0x22, vec![0xF1, 0x95]),
            ]
        );
    }

    #[test]
    fn a_failed_exit_marker_commit_ends_the_job_before_request_transfer_exit() {
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        let mut journal = counting_journal(&commits, Some(13));
        let result = run_flash(&mut host, &mut journal);
        assert!(
            matches!(result, Err(JobError::Journal(JournalError::Io(_)))),
            "{result:?}"
        );
        assert_eq!(host.sent().last(), Some(&Sent::Block));
        assert!(
            !host
                .sent()
                .iter()
                .any(|sent| *sent == Sent::Service(0x37, vec![0x01]))
        );
    }

    #[test]
    fn a_failed_block_commit_ends_the_job_before_the_next_block() {
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        // The second block's commit.
        let mut journal = counting_journal(&commits, Some(9));
        assert!(matches!(
            run_flash(&mut host, &mut journal),
            Err(JobError::Journal(JournalError::Io(_)))
        ));
        let blocks = host.sent().iter().filter(|s| **s == Sent::Block).count();
        assert_eq!(blocks, 2);
    }

    #[test]
    fn an_unreadable_identity_ends_the_job_before_the_erase() {
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        host.software_version = None;
        let mut journal = counting_journal(&commits, None);
        let result = run_flash(&mut host, &mut journal);
        assert!(
            matches!(
                result,
                Err(JobError::IdentityUnreadable {
                    pc: ENTRY,
                    identity: IdentityKind::SoftwareVersion
                })
            ),
            "{result:?}"
        );
        assert!(!host.sent().iter().any(|s| matches!(s, Sent::Routine(_))));
        let facts = &journal.journal().state().facts;
        assert_eq!(
            facts.ecu_hardware_part_number.as_deref(),
            Some(&b"HW01"[..])
        );
        assert!(facts.transfer.is_none());
    }

    /// The done-when case: a journal file written by a job reads back with the markers, the
    /// rising blocks, the identity and the VM state at the entry boundary.
    #[test]
    fn a_journaled_job_reads_back_from_its_file() {
        let dir = std::env::temp_dir().join(format!(
            "ngr-runner-journal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let setup = JournalSetup {
            dir: dir.clone(),
            key: job_key(),
            sources: identity_sources(),
            vin: None,
        };
        let mut journal = JobJournal::create(setup).unwrap();
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        let result = run_flash(&mut host, &mut journal);
        drop(journal);
        let state = crate::journal::Journal::read(&dir, &job_key());
        let _ = std::fs::remove_dir_all(&dir);
        result.unwrap();
        let state = state.unwrap();
        let facts = &state.facts;
        assert_eq!(
            facts.ecu_hardware_part_number.as_deref(),
            Some(&b"HW01"[..])
        );
        assert_eq!(
            facts.pre_erase_software_version.as_deref(),
            Some(&b"SW01"[..])
        );
        let transfer = facts.transfer.as_ref().unwrap();
        assert_eq!(transfer.stage, crate::journal::StageId(7));
        // Instructions 0 to 3 ran before the erase, one step each.
        assert_eq!(
            transfer.started_at,
            StepRef {
                pc: ERASE,
                steps: 4
            }
        );
        assert_eq!(transfer.last_block, Some(3));
        let exit = transfer.exit.as_ref().unwrap();
        assert_eq!(
            exit.intent_at,
            StepRef {
                pc: EXIT,
                steps: 16
            }
        );
        assert!(exit.complete);
        // The last step is RequestTransferExit's, which counts as post-transfer progress.
        assert_eq!(
            facts.last_step,
            Some(StepRef {
                pc: EXIT,
                steps: 16
            })
        );
        assert_eq!(
            exit.last_post_step,
            Some(StepRef {
                pc: EXIT,
                steps: 16
            })
        );
        // The VM state at the entry, on the step before it.
        let (step, bytes) = state.last_vm_state.as_ref().unwrap();
        assert_eq!(
            *step,
            StepRef {
                pc: ENTRY - 1,
                steps: 2
            }
        );
        let vm_state: VmState = postcard::from_bytes(bytes).unwrap();
        assert_eq!((vm_state.pc, vm_state.steps), (ENTRY, 3));
        assert!(vm_state.stack.is_empty());
    }

    /// An erase or RequestTransferExit that the VM refuses before it reaches the host (here an
    /// operand of the wrong type) fails with no marker before it (ADR-233 item 3).
    #[test]
    fn a_refused_boundary_instruction_gets_no_marker() {
        for (operand, boundary) in [(ERASE - 1, ERASE), (EXIT - 1, EXIT)] {
            let mut program = flash_program();
            program.code[operand as usize] = Op::PushI64(1);
            let commits = Rc::new(Cell::new(0));
            let mut host = FlashHost::new(Rc::clone(&commits));
            let mut journal = counting_journal(&commits, None);
            let result = run_on(
                &program,
                &mut host,
                JobLimits::default(),
                &AtomicBool::new(false),
                Some(&mut journal),
            );
            assert!(
                matches!(result, Err(JobError::Vm { pc, .. }) if pc == boundary),
                "{result:?}"
            );
            let facts = &journal.journal().state().facts;
            if boundary == ERASE {
                assert!(facts.transfer.is_none());
                assert!(!host.sent().iter().any(|s| matches!(s, Sent::Routine(_))));
            } else {
                assert!(facts.transfer.as_ref().unwrap().exit.is_none());
                assert!(!host.sent().contains(&Sent::Service(0x37, vec![0x01])));
            }
        }
    }

    /// A job cancelled during an identity read sends no further request: neither the next
    /// identity read nor the boundary's own instruction (the runner's cancel contract).
    #[test]
    fn a_cancel_during_the_identity_reads_stops_before_the_next_request() {
        // With the entry at the erase, the erase is the very next instruction after the reads.
        let mut at_erase = flash_program();
        at_erase.flash[0].boundaries.entry_pc = ERASE;
        for program in [flash_program(), at_erase] {
            for did in [[0xF1, 0x91], [0xF1, 0x95]] {
                let flag = Arc::new(AtomicBool::new(false));
                let commits = Rc::new(Cell::new(0));
                let mut host = FlashHost::new(Rc::clone(&commits));
                host.cancel_on_read = Some((did, Arc::clone(&flag)));
                let mut journal = counting_journal(&commits, None);
                let result = run_on(
                    &program,
                    &mut host,
                    JobLimits::default(),
                    &flag,
                    Some(&mut journal),
                );
                assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
                assert_eq!(
                    host.sent().last(),
                    Some(&Sent::Service(0x22, did.to_vec())),
                    "{did:02X?}"
                );
                assert!(!host.sent().iter().any(|s| matches!(s, Sent::Routine(_))));
            }
        }
    }

    /// `flash_program` with a post-transfer CheckMemory routine (0xFF01) after the exit, and the
    /// plan's end after it.
    fn program_with_check(recovery_required: diag_ir::RecoveryRequired) -> (Program, u32) {
        let mut program = flash_program();
        // RequestTransferExit, its Pop, then this routine's operand.
        let check = EXIT + 3;
        program.code.extend([
            Op::PushBytes(0),
            Op::RoutineControl {
                routine: 0xFF01,
                sub: 1,
            },
            Op::Pop,
        ]);
        program.flash[0].boundaries.post_transfer_end_pc = check + 2;
        program.flash[0].recovery_required = recovery_required;
        program.validate().unwrap();
        (program, check)
    }

    /// The done-when cases, end to end: a job whose erase response is lost leaves a journal that
    /// classifies as an interrupted transfer; one whose response to the request at the
    /// recovery-required point is lost leaves one that classifies as on-site intervention,
    /// because the request's intent was journaled before it was sent (ADR-253).
    #[test]
    fn a_lost_response_classifies_from_the_journal_the_job_left() {
        use crate::restart::{OnSiteReason, RestartDecision, classify};

        let (program, check) = program_with_check(diag_ir::RecoveryRequired::Never);
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        host.lose_routine = Some(0xFF00);
        let mut journal = counting_journal(&commits, None);
        let result = run_on(
            &program,
            &mut host,
            JobLimits::default(),
            &AtomicBool::new(false),
            Some(&mut journal),
        );
        assert!(
            matches!(result, Err(JobError::Host { pc: ERASE, .. })),
            "{result:?}"
        );
        let decision = classify(&program, Ok(journal.journal().state()));
        let RestartDecision::Restart(point) = decision else {
            panic!("{decision:?}");
        };
        assert_eq!(
            point.interrupted_at,
            Some(StepRef {
                pc: ERASE,
                steps: 4
            })
        );
        assert_eq!(point.entry_state.pc, ENTRY);

        let (program, check2) = program_with_check(diag_ir::RecoveryRequired::FromPc(check));
        assert_eq!(check, check2);
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        host.lose_routine = Some(0xFF01);
        let mut journal = counting_journal(&commits, None);
        let result = run_on(
            &program,
            &mut host,
            JobLimits::default(),
            &AtomicBool::new(false),
            Some(&mut journal),
        );
        assert!(
            matches!(result, Err(JobError::Host { pc, .. }) if pc == check),
            "{result:?}"
        );
        // The intent is the commit right before the routine was sent.
        let (_, commits_before) = host
            .log
            .iter()
            .find(|(sent, _)| *sent == Sent::Routine(0xFF01))
            .unwrap();
        assert_eq!(
            journal.journal().state().facts.last_intent.map(|at| at.pc),
            Some(check)
        );
        assert_eq!(*commits_before, commits.get());
        assert!(matches!(
            classify(&program, Ok(journal.journal().state())),
            RestartDecision::OnSiteInterventionRequired(OnSiteReason::RecoveryRequiredPoint {
                flash_session: 1,
                ..
            })
        ));
    }

    /// A recovery-required point at the erase or at RequestTransferExit is covered by its marker
    /// and adds no intent; one past them adds exactly one, before the first primitive there.
    #[test]
    fn the_recovery_point_is_written_ahead_once() {
        let count = |program: &Program| {
            let commits = Rc::new(Cell::new(0));
            let mut host = FlashHost::new(Rc::clone(&commits));
            let mut journal = counting_journal(&commits, None);
            run_on(
                program,
                &mut host,
                JobLimits::default(),
                &AtomicBool::new(false),
                Some(&mut journal),
            )
            .unwrap();
            (commits.get(), journal.journal().state().facts.last_intent)
        };
        let (plain, _) = program_with_check(diag_ir::RecoveryRequired::Never);
        let (baseline, none) = count(&plain);
        assert_eq!(none, None);
        for from in [ERASE, EXIT] {
            let (program, _) = program_with_check(diag_ir::RecoveryRequired::FromPc(from));
            assert_eq!(count(&program), (baseline, None), "from {from}");
        }
        // From the RequestDownload: one intent there, none for the blocks after it.
        let download = ERASE + 3;
        let (program, _) = program_with_check(diag_ir::RecoveryRequired::FromPc(download));
        let (commits, intent) = count(&program);
        assert_eq!(commits, baseline + 1);
        assert_eq!(intent.map(|at| at.pc), Some(download));
        // From an instruction that is not a primitive (the check's operand): the intent lands on
        // the next primitive, the check.
        let (program, check) = program_with_check(diag_ir::RecoveryRequired::Never);
        let mut program = program;
        program.flash[0].recovery_required = diag_ir::RecoveryRequired::FromPc(check - 1);
        program.validate().unwrap();
        let (commits, intent) = count(&program);
        assert_eq!(commits, baseline + 1);
        assert_eq!(intent.map(|at| at.pc), Some(check));
    }

    /// Two plans, each with its own recovery-required point: each writes its own intent once.
    #[test]
    fn each_plan_writes_its_recovery_point_ahead() {
        let mut program = flash_program();
        let len = program.code.len() as u32;
        let plan_code: Vec<Op> = program.code[ENTRY as usize..].to_vec();
        program.code.extend(plan_code);
        let download = ERASE + 3;
        program.flash[0].recovery_required = diag_ir::RecoveryRequired::FromPc(download);
        let mut second = program.flash[0].clone();
        second.flash_session = 2;
        second.stage = 8;
        let shift = len - ENTRY;
        let b = &mut second.boundaries;
        b.entry_pc += shift;
        b.erase_pc += shift;
        b.transfer_exit_pc += shift;
        b.post_transfer_end_pc += shift;
        second.recovery_required = diag_ir::RecoveryRequired::FromPc(download + shift);
        program.flash.push(second);
        program.validate().unwrap();
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        let mut journal = counting_journal(&commits, None);
        run_on(
            &program,
            &mut host,
            JobLimits::default(),
            &AtomicBool::new(false),
            Some(&mut journal),
        )
        .unwrap();
        // The adjacent-plans count (16 + 12) plus one intent per plan.
        assert_eq!(commits.get(), 16 + 12 + 2);
        assert_eq!(
            journal.journal().state().facts.last_intent.map(|at| at.pc),
            Some(download + shift)
        );
    }

    /// A post-transfer step is recorded, and the completion is committed with the step that
    /// reaches the end, even when the job stops right there (here at its step limit).
    #[test]
    fn the_post_transfer_steps_and_their_completion_are_recorded() {
        let mut program = flash_program();
        // RequestTransferExit, its Pop, then this routine's operand.
        let check_memory = EXIT + 3;
        program.code.extend([
            Op::PushBytes(0),
            Op::RoutineControl {
                routine: 0xFF01,
                sub: 1,
            },
            Op::Pop,
            // After the plan.
            Op::PushI64(1),
            Op::Pop,
        ]);
        let end = check_memory + 2;
        program.flash[0].boundaries.post_transfer_end_pc = end;
        program.validate().unwrap();
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        let mut journal = counting_journal(&commits, None);
        let limits = JobLimits {
            // Every instruction is one step, so the job stops on arriving at the end.
            max_steps: u64::from(end),
            ..JobLimits::default()
        };
        let result = run_on(
            &program,
            &mut host,
            limits,
            &AtomicBool::new(false),
            Some(&mut journal),
        );
        assert!(matches!(result, Err(JobError::StepLimit(_))), "{result:?}");
        let facts = &journal.journal().state().facts;
        let exit = facts.transfer.as_ref().unwrap().exit.as_ref().unwrap();
        assert_eq!(
            exit.last_post_step,
            Some(StepRef {
                pc: check_memory,
                steps: u64::from(check_memory)
            })
        );
        assert!(exit.complete);
    }

    /// The software version answered with the plan's declared "no valid application" response
    /// is not recorded, and the job goes on; another negative response ends it.
    #[test]
    fn an_ecu_without_an_application_is_flashed_without_a_version() {
        let mut program = flash_program();
        program.flash[0].no_application = Some(diag_ir::NoApplication::Nrc(0x31));
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        host.software_version = None;
        let mut journal = counting_journal(&commits, None);
        run_on(
            &program,
            &mut host,
            JobLimits::default(),
            &AtomicBool::new(false),
            Some(&mut journal),
        )
        .unwrap();
        let facts = &journal.journal().state().facts;
        assert_eq!(facts.pre_erase_software_version, None);
        assert_eq!(
            facts.ecu_hardware_part_number.as_deref(),
            Some(&b"HW01"[..])
        );
        assert!(
            facts
                .transfer
                .as_ref()
                .unwrap()
                .exit
                .as_ref()
                .unwrap()
                .complete
        );

        program.flash[0].no_application = Some(diag_ir::NoApplication::Nrc(0x22));
        let mut host = FlashHost::new(Rc::clone(&commits));
        host.software_version = None;
        let mut journal = counting_journal(&commits, None);
        let result = run_on(
            &program,
            &mut host,
            JobLimits::default(),
            &AtomicBool::new(false),
            Some(&mut journal),
        );
        assert!(
            matches!(
                result,
                Err(JobError::IdentityUnreadable {
                    identity: IdentityKind::SoftwareVersion,
                    ..
                })
            ),
            "{result:?}"
        );
    }

    /// Arriving at the entry again before the transfer (a loop the validator allows) does not
    /// read the identity a second time.
    #[test]
    fn the_identity_is_read_once_per_job() {
        let program = flash_program();
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        let mut journal = counting_journal(&commits, None);
        let mut vm = Vm::new(&program);
        vm.state.pc = ENTRY;
        let running = AtomicBool::new(false);
        journal
            .arrive(&program, &vm.state, &mut host, &running)
            .unwrap();
        vm.state.steps = 10;
        journal
            .arrive(&program, &vm.state, &mut host, &running)
            .unwrap();
        let reads = host
            .sent()
            .iter()
            .filter(|sent| matches!(sent, Sent::Service(0x22, _)))
            .count();
        assert_eq!(reads, 2);
        assert_eq!(commits.get(), 2);
    }

    /// An entry that is also the erase: the step into it carries the VM state, then the
    /// identity and the transfer-start marker follow, in that order.
    #[test]
    fn an_entry_at_the_erase_records_the_state_then_the_marker() {
        let mut program = flash_program();
        program.flash[0].boundaries.entry_pc = ERASE;
        program.validate().unwrap();
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        let mut journal = counting_journal(&commits, None);
        run_on(
            &program,
            &mut host,
            JobLimits::default(),
            &AtomicBool::new(false),
            Some(&mut journal),
        )
        .unwrap();
        let state = journal.journal().state();
        let (step, _) = state.last_vm_state.as_ref().unwrap();
        assert_eq!(
            *step,
            StepRef {
                pc: ERASE - 1,
                steps: 3
            }
        );
        let transfer = state.facts.transfer.as_ref().unwrap();
        assert_eq!(
            transfer.started_at,
            StepRef {
                pc: ERASE,
                steps: 4
            }
        );
        assert_eq!(transfer.last_block, Some(3));
    }

    /// A job that starts at the plan's entry has its initial state there: no step record.
    #[test]
    fn a_job_that_starts_at_the_entry_records_no_state() {
        let mut program = flash_program();
        // Drop the programming session: the plan starts the program.
        program.code.drain(..3);
        let plan = &mut program.flash[0].boundaries;
        plan.entry_pc -= 3;
        plan.erase_pc -= 3;
        plan.transfer_exit_pc -= 3;
        plan.post_transfer_end_pc -= 3;
        program.validate().unwrap();
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        let mut journal = counting_journal(&commits, None);
        run_on(
            &program,
            &mut host,
            JobLimits::default(),
            &AtomicBool::new(false),
            Some(&mut journal),
        )
        .unwrap();
        let state = journal.journal().state();
        assert_eq!(state.last_vm_state, None);
        assert_eq!(state.facts.transfer.as_ref().unwrap().last_block, Some(3));
        // Identity 2, transfer start 1, erase and RequestDownload steps 2, blocks and their
        // steps 6, exit 1, its step 1, completion 1.
        assert_eq!(commits.get(), 14);
    }

    /// Two plans where the first one's end is the second one's entry: the first completes before
    /// the second starts, and the identity is not read again for the second.
    #[test]
    fn adjacent_plans_complete_one_before_starting_the_next() {
        let mut program = flash_program();
        let len = program.code.len() as u32;
        let plan_code: Vec<Op> = program.code[ENTRY as usize..].to_vec();
        program.code.extend(plan_code);
        let mut second = program.flash[0].clone();
        second.flash_session = 2;
        second.stage = 8;
        let shift = len - ENTRY;
        let b = &mut second.boundaries;
        b.entry_pc += shift;
        b.erase_pc += shift;
        b.transfer_exit_pc += shift;
        b.post_transfer_end_pc += shift;
        program.flash.push(second);
        program.validate().unwrap();
        let commits = Rc::new(Cell::new(0));
        let mut host = FlashHost::new(Rc::clone(&commits));
        let mut journal = counting_journal(&commits, None);
        run_on(
            &program,
            &mut host,
            JobLimits::default(),
            &AtomicBool::new(false),
            Some(&mut journal),
        )
        .unwrap();
        // The first plan: 14 records up to RequestTransferExit's step, then the step into the
        // second plan's entry (with the VM state) and the first plan's completion. The second:
        // transfer start, 2 steps, 6 for the blocks, exit marker, its step, completion.
        assert_eq!(commits.get(), 16 + 12);
        let routines: Vec<u64> = host
            .log
            .iter()
            .filter(|(sent, _)| matches!(sent, Sent::Routine(_)))
            .map(|(_, commits)| *commits)
            .collect();
        assert_eq!(routines, [4, 17]);
        let reads = host
            .sent()
            .iter()
            .filter(|sent| matches!(sent, Sent::Service(0x22, _)))
            .count();
        assert_eq!(reads, 2);
        let transfer = journal.journal().state().facts.transfer.clone().unwrap();
        assert_eq!(transfer.stage, crate::journal::StageId(8));
        assert!(transfer.exit.unwrap().complete);
    }

    /// A link that fails to open leaves no journal behind, so the job can be tried again.
    #[test]
    fn a_job_whose_link_fails_leaves_no_journal() {
        let dir = std::env::temp_dir().join(format!(
            "ngr-runner-link-fails-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let setup = JournalSetup {
            dir: dir.clone(),
            key: job_key(),
            sources: identity_sources(),
            vin: None,
        };
        let slot = vci_only_slot("link-fails");
        let result = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let handle = Handle::current();
                tokio::task::spawn_blocking(move || {
                    run_job(
                        handle,
                        unreachable_client(),
                        &LinkConfig::iso15765(0x7E0, 0x7E8),
                        &flash_program(),
                        JobLimits::default(),
                        &AtomicBool::new(false),
                        Some(JournalMode::Create(setup)),
                        slot,
                    )
                })
                .await
                .unwrap()
            });
        let left = std::fs::read_dir(&dir).unwrap().count();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
        assert_eq!(left, 0);
    }

    /// A resume whose link fails neither opens nor creates a journal: the job that ran before
    /// keeps the journal it left.
    #[test]
    fn a_resume_whose_link_fails_leaves_the_journal_as_it_was() {
        let program = flash_program();
        let dir = journal_dir("resume-link-fails");
        interrupted(&program, &dir);
        let before = std::fs::read(journal_file(&dir)).unwrap();
        let guards = take_guards(&dir);
        let (result, _guards) =
            resume_on_unreachable(program, &dir, guards, Arc::new(AtomicBool::new(false)));
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
        assert_eq!(std::fs::read(journal_file(&dir)).unwrap(), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn guard_setup(dir: &std::path::Path) -> crate::guards::GuardSetup {
        crate::guards::GuardSetup {
            dir: dir.join("locks"),
            vci: "VCI-1".to_owned(),
        }
    }

    /// A slot holding VCI-only guards in `dir`'s lock directory, which the test removes with
    /// `dir`: a restart whose VIN matches sweeps the 4096 vehicle lock files into it.
    fn dir_slot(dir: &std::path::Path) -> GuardSlot {
        let guards = JobGuards::take_vci_only(
            &guard_setup(dir),
            Duration::from_millis(1),
            &AtomicBool::new(false),
        )
        .unwrap();
        Arc::new(Mutex::new(Some(guards)))
    }

    /// A slot holding VCI-only guards in a lock directory of its own, for a test that calls
    /// `run_job` directly.
    fn vci_only_slot(tag: &str) -> GuardSlot {
        Arc::new(Mutex::new(Some(vci_only_guards(tag))))
    }

    fn vci_only_guards(tag: &str) -> JobGuards {
        let dir = std::env::temp_dir().join(format!(
            "ngr-runner-guards-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        JobGuards::take_vci_only(
            &crate::guards::GuardSetup {
                dir,
                vci: "VCI-1".to_owned(),
            },
            Duration::from_millis(1),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    /// Guards with the reprogramming slot, in a lock directory of their own.
    #[cfg(debug_assertions)]
    fn full_guards(tag: &str) -> JobGuards {
        let dir = std::env::temp_dir().join(format!(
            "ngr-runner-full-guards-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        JobGuards::take(
            &crate::guards::GuardSetup {
                dir,
                vci: "VCI-1".to_owned(),
            },
            Duration::from_millis(1),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    fn take_guards(dir: &std::path::Path) -> JobGuards {
        JobGuards::take(
            &guard_setup(dir),
            Duration::from_millis(1),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    /// `run_job` resuming the job of `dir`'s journal on a worker that cannot be reached, with
    /// the guard slot `resume_program_journaled` builds; gives the guards back with the result.
    fn resume_on_unreachable(
        program: Program,
        dir: &std::path::Path,
        guards: JobGuards,
        cancelled: Arc<AtomicBool>,
    ) -> (Result<VmState, JobError>, JobGuards) {
        let setup = file_setup(dir);
        let slot: GuardSlot = Arc::new(Mutex::new(Some(guards)));
        let job_slot = Arc::clone(&slot);
        let result = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let handle = Handle::current();
                tokio::task::spawn_blocking(move || {
                    run_job(
                        handle,
                        unreachable_client(),
                        &LinkConfig::iso15765(0x7E0, 0x7E8),
                        &program,
                        JobLimits::default(),
                        &cancelled,
                        Some(JournalMode::Resume(setup)),
                        job_slot,
                    )
                })
                .await
                .unwrap()
            });
        let guards = slot.lock().unwrap().take().unwrap();
        (result, guards)
    }

    /// The done-when cases (ADR-256): the guards are held for the whole run and come back with
    /// its result, so a job that survived a worker crash runs again on them, while a new run of
    /// the same job waits for them in `JobGuards::take` and so never reaches a link until the
    /// first one lets go.
    #[test]
    fn a_resumed_job_runs_on_guards_it_keeps_across_runs() {
        let program = flash_program();
        let dir = journal_dir("resume-guards");
        interrupted(&program, &dir);
        let guards = take_guards(&dir);

        // A run that fails (here at the link) gives the guards back, still held.
        let (result, guards) = resume_on_unreachable(
            program.clone(),
            &dir,
            guards,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");

        // A new run of the job, as after an agent crash, waits for them while they are held.
        let cancelled = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (dir, cancelled) = (dir.clone(), Arc::clone(&cancelled));
            std::thread::spawn(move || {
                JobGuards::take(&guard_setup(&dir), Duration::from_millis(1), &cancelled).map(drop)
            })
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(!waiter.is_finished(), "a new run waits for the guards");
        cancelled.store(true, Ordering::Relaxed);
        assert!(matches!(
            waiter.join().unwrap(),
            Err(crate::guards::GuardError::Cancelled)
        ));

        // The job that holds them runs again on them.
        let (result, guards) = resume_on_unreachable(
            program.clone(),
            &dir,
            guards,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");

        // Once it lets go, a new run takes them.
        drop(guards);
        let (result, _guards) = resume_on_unreachable(
            program,
            &dir,
            take_guards(&dir),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `resume_program_journaled` gives the guards back, still held, also when it returns before
    /// the job starts (here: a current-thread runtime).
    #[tokio::test(flavor = "current_thread")]
    async fn a_resume_that_cannot_start_gives_the_guards_back() {
        let dir = journal_dir("resume-early");
        let (result, guards) = resume_program_journaled(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            flash_program(),
            JobLimits::default(),
            file_setup(&dir),
            take_guards(&dir),
        )
        .await;
        assert!(
            matches!(result, Err(JobError::CurrentThreadRuntime)),
            "{result:?}"
        );
        // Still held: another taker waits until it is cancelled.
        let cancelled = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (dir, cancelled) = (dir.clone(), Arc::clone(&cancelled));
            std::thread::spawn(move || {
                JobGuards::take(&guard_setup(&dir), Duration::from_millis(1), &cancelled).map(drop)
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "the guards are still held");
        cancelled.store(true, Ordering::Relaxed);
        assert!(waiter.join().unwrap().is_err());
        drop(guards);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_program_without_a_plan_keeps_no_journal() {
        let dir =
            std::env::temp_dir().join(format!("ngr-runner-no-journal-{}", std::process::id()));
        let setup = JournalSetup {
            dir: dir.clone(),
            key: job_key(),
            sources: identity_sources(),
            vin: None,
        };
        // Fails when the link opens, before the journal would be created: no file appears.
        let slot = vci_only_slot("no-journal");
        let result = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let handle = Handle::current();
                tokio::task::spawn_blocking(move || {
                    run_job(
                        handle,
                        unreachable_client(),
                        &LinkConfig::iso15765(0x7E0, 0x7E8),
                        &program(two_requests()),
                        JobLimits::default(),
                        &AtomicBool::new(false),
                        Some(JournalMode::Create(setup)),
                        slot,
                    )
                })
                .await
                .unwrap()
            });
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
        assert!(!dir.exists());
    }

    // ------------------------------------------------------------ resume (ADR-255)

    /// `flash_program` with a declared supply-voltage range of 11 to 15 V, read from the VCI.
    fn flash_program_with_voltage() -> Program {
        let mut program = flash_program();
        let vbatt = Some(diag_ir::Source::RuntimeInput(
            diag_ir::RuntimeInput::SupplyVoltageMillivolts,
        ));
        program.preconditions.voltage_mv = Some(diag_ir::Precondition {
            satisfied: diag_ir::Satisfied {
                lower: 11_000,
                upper: 15_000,
            },
            default_session: vbatt,
            programming_session: vbatt,
        });
        program.validate().expect("the fixture is a valid program");
        program
    }

    fn journal_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ngr-runner-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn target() -> crate::journal::Vin {
        crate::journal::Vin::new(TARGET_VIN.to_owned())
    }

    fn file_setup(dir: &std::path::Path) -> JournalSetup {
        file_setup_for(dir, Some(TARGET_VIN))
    }

    /// The setup of a job that targets `vin`.
    fn file_setup_for(dir: &std::path::Path, vin: Option<&str>) -> JournalSetup {
        JournalSetup {
            dir: dir.to_owned(),
            key: job_key(),
            sources: identity_sources(),
            vin: vin.map(|vin| crate::journal::Vin::new(vin.to_owned())),
        }
    }

    /// A first run that writes its journal in `dir`, on a host `prepare` sets up.
    fn first_run(
        program: &Program,
        dir: &std::path::Path,
        limits: JobLimits,
        prepare: impl FnOnce(&mut FlashHost),
    ) -> Result<VmState, JobError> {
        first_run_for(program, dir, limits, Some(TARGET_VIN), prepare)
    }

    /// [`first_run`] for a job that targets `vin`.
    fn first_run_for(
        program: &Program,
        dir: &std::path::Path,
        limits: JobLimits,
        vin: Option<&str>,
        prepare: impl FnOnce(&mut FlashHost),
    ) -> Result<VmState, JobError> {
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        prepare(&mut host);
        let mut journal = JobJournal::create(file_setup_for(dir, vin)).unwrap();
        run_on(
            program,
            &mut host,
            limits,
            &AtomicBool::new(false),
            Some(&mut journal),
        )
    }

    /// A first run whose erase response is lost: an interrupted transfer.
    fn interrupted(program: &Program, dir: &std::path::Path) {
        interrupted_for(program, dir, Some(TARGET_VIN));
    }

    /// [`interrupted`] for a job that targets `vin`.
    fn interrupted_for(program: &Program, dir: &std::path::Path, vin: Option<&str>) {
        let result = first_run_for(program, dir, JobLimits::default(), vin, |host| {
            host.lose_routine = Some(0xFF00);
        });
        assert!(
            matches!(result, Err(JobError::Host { pc: ERASE, .. })),
            "{result:?}"
        );
    }

    fn resume(
        program: &Program,
        dir: &std::path::Path,
        host: &mut FlashHost,
        cancelled: bool,
    ) -> Result<VmState, JobError> {
        resume_for(program, dir, host, cancelled, Some(TARGET_VIN))
    }

    /// [`resume`] for a job that targets `vin`.
    fn resume_for(
        program: &Program,
        dir: &std::path::Path,
        host: &mut FlashHost,
        cancelled: bool,
        vin: Option<&str>,
    ) -> Result<VmState, JobError> {
        resume_in(program, dir, host, cancelled, vin, &dir_slot(dir))
    }

    /// [`resume_for`] with the job's guard slot given.
    fn resume_in(
        program: &Program,
        dir: &std::path::Path,
        host: &mut FlashHost,
        cancelled: bool,
        vin: Option<&str>,
        slot: &GuardSlot,
    ) -> Result<VmState, JobError> {
        resume_on(
            program,
            host,
            JobLimits::default(),
            &AtomicBool::new(cancelled),
            Journal::open(dir, &job_key()),
            identity_sources(),
            vin.map(|vin| crate::journal::Vin::new(vin.to_owned()))
                .as_ref(),
            slot,
        )
    }

    fn read_of(did: [u8; 2]) -> Sent {
        Sent::Service(0x22, did.to_vec())
    }

    /// The journal file in `dir`, beside its writer lock's sidecar.
    fn journal_file(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "journal"))
            .expect("a journal file")
    }

    fn resumes(dir: &std::path::Path) -> u16 {
        Journal::read(dir, &job_key())
            .unwrap()
            .facts
            .resume_count(crate::journal::StageId(7))
    }

    /// The done-when cases: a restart commits its resume before anything reaches the ECU, and
    /// each restart of a job that keeps crashing in recovery counts one more until the limit
    /// stops it, all on the journal alone (no attempt key).
    #[test]
    fn each_restart_counts_a_resume_until_the_limit_with_nothing_sent() {
        let program = flash_program_with_voltage();
        let dir = journal_dir("resume-limit");
        interrupted(&program, &dir);

        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        host.voltage = Some(12_600);
        let result = resume(&program, &dir, &mut host, false);
        assert!(
            matches!(
                result,
                Err(JobError::OnSiteInterventionRequired(
                    OnSiteReason::RestartOrderUnavailable {
                        flash_session: 1,
                        teardown: restart::Teardown::Reset,
                        ..
                    }
                ))
            ),
            "{result:?}"
        );
        // Only the gates' ReadDataByIdentifier requests: the VIN, then the hardware identity.
        assert_eq!(
            host.sent(),
            [read_of([0xF1, 0x90]), read_of([0xF1, 0x91]), reset_sent()]
        );
        // Read by step 1 and again by the gates.
        assert_eq!(host.voltage_reads, 2);
        assert_eq!(resumes(&dir), 1);
        let facts = Journal::read(&dir, &job_key()).unwrap().facts;
        assert_eq!(facts.attempt_key, None);

        // The agent crashed again during that recovery: the plan allows one resume.
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        host.voltage = Some(12_600);
        let result = resume(&program, &dir, &mut host, false);
        assert!(
            matches!(
                result,
                Err(JobError::OnSiteInterventionRequired(
                    OnSiteReason::ResumeLimitReached {
                        flash_session: 1,
                        resumes: 1,
                        max: 1
                    }
                ))
            ),
            "{result:?}"
        );
        assert_eq!(host.sent(), []);
        assert_eq!(host.voltage_reads, 0);
        assert_eq!(resumes(&dir), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A voltage outside the declared range, or none from the VCI, stops the restart before the
    /// count; a program that declares no range is not checked.
    #[test]
    fn the_supply_voltage_is_checked_before_the_resume_is_counted() {
        let program = flash_program_with_voltage();
        for voltage in [Some(10_999), Some(15_001), None] {
            let dir = journal_dir("resume-voltage");
            interrupted(&program, &dir);
            let mut host = FlashHost::new(Rc::new(Cell::new(0)));
            host.voltage = voltage;
            let result = resume(&program, &dir, &mut host, false);
            assert!(
                matches!(
                    result,
                    Err(JobError::OnSiteInterventionRequired(
                        OnSiteReason::SupplyVoltage { flash_session: 1, millivolts }
                    )) if millivolts == voltage
                ),
                "{voltage:?}: {result:?}"
            );
            assert_eq!(host.sent(), []);
            assert_eq!(resumes(&dir), 0);
            std::fs::remove_dir_all(&dir).unwrap();
        }

        // The range is inclusive at both ends.
        for millivolts in [11_000, 15_000] {
            let dir = journal_dir("resume-voltage-edge");
            interrupted(&program, &dir);
            let mut host = FlashHost::new(Rc::new(Cell::new(0)));
            host.voltage = Some(millivolts);
            let result = resume(&program, &dir, &mut host, false);
            assert!(
                matches!(
                    result,
                    Err(JobError::OnSiteInterventionRequired(
                        OnSiteReason::RestartOrderUnavailable { .. }
                    ))
                ),
                "{millivolts}: {result:?}"
            );
            assert_eq!(resumes(&dir), 1);
            std::fs::remove_dir_all(&dir).unwrap();
        }

        // A worker that cannot be asked gives no reading.
        let dir = journal_dir("resume-voltage-error");
        interrupted(&program, &dir);
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        host.voltage_fails = true;
        let result = resume(&program, &dir, &mut host, false);
        assert!(
            matches!(
                result,
                Err(JobError::OnSiteInterventionRequired(
                    OnSiteReason::SupplyVoltage {
                        millivolts: None,
                        ..
                    }
                ))
            ),
            "{result:?}"
        );
        assert_eq!(host.voltage_reads, 1);
        assert_eq!(resumes(&dir), 0);
        std::fs::remove_dir_all(&dir).unwrap();

        let program = flash_program();
        let dir = journal_dir("resume-no-voltage");
        interrupted(&program, &dir);
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        let result = resume(&program, &dir, &mut host, false);
        assert!(
            matches!(
                result,
                Err(JobError::OnSiteInterventionRequired(
                    OnSiteReason::RestartOrderUnavailable { .. }
                ))
            ),
            "{result:?}"
        );
        assert_eq!(host.voltage_reads, 0);
        assert_eq!(resumes(&dir), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A cancel stops a restart before the voltage read, during it, or before the commit, with no
    /// resume counted.
    #[test]
    fn a_cancelled_restart_counts_no_resume() {
        let program = flash_program_with_voltage();
        let dir = journal_dir("resume-cancel");
        interrupted(&program, &dir);
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        host.voltage = Some(12_600);
        let result = resume(&program, &dir, &mut host, true);
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
        assert_eq!(host.voltage_reads, 0);
        assert_eq!(host.sent(), []);
        assert_eq!(resumes(&dir), 0);
        std::fs::remove_dir_all(&dir).unwrap();

        // A cancel during the read wins over what the read gave, failed or in range.
        for fails in [true, false] {
            let dir = journal_dir("resume-cancel-during-read");
            interrupted(&program, &dir);
            let cancelled = Arc::new(AtomicBool::new(false));
            let mut host = FlashHost::new(Rc::new(Cell::new(0)));
            host.voltage = Some(12_600);
            host.voltage_fails = fails;
            host.cancel_on_voltage = Some(Arc::clone(&cancelled));
            let result = resume_on(
                &program,
                &mut host,
                JobLimits::default(),
                &cancelled,
                Journal::open(&dir, &job_key()),
                identity_sources(),
                Some(&target()),
                &dir_slot(&dir),
            );
            assert!(
                matches!(result, Err(JobError::Cancelled)),
                "{fails}: {result:?}"
            );
            assert_eq!(host.voltage_reads, 1);
            assert_eq!(resumes(&dir), 0);
            std::fs::remove_dir_all(&dir).unwrap();
        }

        // Without a voltage range, the cancel is caught before the commit.
        let program = flash_program();
        let dir = journal_dir("resume-cancel-no-voltage");
        interrupted(&program, &dir);
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        let result = resume(&program, &dir, &mut host, true);
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
        assert_eq!(resumes(&dir), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---------------------------------------------- restart gates (ADR-229 item 2 step 2)

    /// A first run interrupted at the erase, then a restart of `program` on a host `prepare`
    /// sets up, for a job that targets `vin`. Gives the result and the host.
    fn restart_with(
        program: &Program,
        vin: Option<&str>,
        prepare: impl FnOnce(&mut FlashHost),
    ) -> (Result<VmState, JobError>, FlashHost) {
        let dir = journal_dir("resume-gates");
        interrupted_for(program, &dir, vin);
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        host.voltage = Some(12_600);
        prepare(&mut host);
        let result = resume_for(program, &dir, &mut host, false, vin);
        std::fs::remove_dir_all(&dir).unwrap();
        (result, host)
    }

    /// The teardown of a restart whose gates did not pass for `reason`.
    fn passive_gate(reason: restart::PassiveReason) -> restart::Teardown {
        restart::Teardown::Passive(restart::PassiveCause::Gate(reason))
    }

    /// The teardown a restart ended in. Fails the test on any other result.
    fn teardown_of(result: &Result<VmState, JobError>) -> restart::Teardown {
        match result {
            Err(JobError::OnSiteInterventionRequired(OnSiteReason::RestartOrderUnavailable {
                flash_session: 1,
                teardown,
                ..
            })) => teardown.clone(),
            other => panic!("{other:?}"),
        }
    }

    /// `flash_program` declaring the engine precondition (a flag that must be 0) with these
    /// sources.
    fn flash_program_with_engine(
        default_session: diag_ir::Source,
        programming_session: diag_ir::Source,
    ) -> Program {
        let mut program = flash_program();
        program.preconditions.engine = Some(diag_ir::Precondition {
            satisfied: diag_ir::Satisfied { lower: 0, upper: 0 },
            default_session: Some(default_session),
            programming_session: Some(programming_session),
        });
        program.validate().expect("the fixture is a valid program");
        program
    }

    fn input(input: diag_ir::RuntimeInput) -> diag_ir::Source {
        diag_ir::Source::RuntimeInput(input)
    }

    fn reset_sent() -> Sent {
        Sent::Service(0x11, vec![0x01])
    }

    /// Nothing but ReadDataByIdentifier requests, then at most the teardown's one ECUReset (last),
    /// was sent: no routine, no block.
    fn assert_reads_then_reset(host: &FlashHost) {
        let sent = host.sent();
        let reads = sent.strip_suffix(&[reset_sent()]).unwrap_or(&sent);
        assert!(
            reads
                .iter()
                .all(|sent| matches!(sent, Sent::Service(0x22, _))),
            "{sent:?}"
        );
    }

    /// Nothing but ReadDataByIdentifier requests was sent: no ECUReset, no routine, no block.
    fn assert_only_reads(host: &FlashHost) {
        assert!(
            host.sent()
                .iter()
                .all(|sent| matches!(sent, Sent::Service(0x22, _))),
            "{:?}",
            host.sent()
        );
    }

    /// A VIN that decodes to another vehicle's aborts the job; nothing after the VIN read is
    /// sent, and the error does not carry the VIN.
    #[test]
    fn a_vin_mismatch_aborts_the_restart_after_the_vin_read() {
        let (result, host) = restart_with(&flash_program(), Some(TARGET_VIN), |host| {
            host.vin = Some(b"WDB99999999999999".to_vec());
        });
        let Err(error @ JobError::IdentityMismatch { identity }) = &result else {
            panic!("{result:?}");
        };
        assert_eq!(*identity, IdentityKind::Vin);
        let text = format!("{error} {error:?}");
        let bucket = format!("{:03x}", crate::guards::vehicle_bucket(&target()));
        assert!(!text.contains("WDB"), "{text}");
        assert!(!text.contains("vehicle-"), "{text}");
        assert!(!text.contains(&bucket), "{text}");
        assert_eq!(host.sent(), [read_of([0xF1, 0x90])]);
    }

    #[test]
    fn a_vin_that_cannot_be_read_leaves_the_teardown_passive() {
        let passive = passive_gate(restart::PassiveReason::VinNotEstablished);
        // A negative response.
        let (result, host) = restart_with(&flash_program(), Some(TARGET_VIN), |host| {
            host.vin = None;
        });
        assert_eq!(teardown_of(&result), passive);
        assert_eq!(host.sent(), [read_of([0xF1, 0x90])]);

        // A VIN field that is blank, so not text.
        let (result, _) = restart_with(&flash_program(), Some(TARGET_VIN), |host| {
            host.vin = Some(vec![b' '; 17]);
        });
        assert_eq!(teardown_of(&result), passive);

        // The program declares no VIN source.
        let mut no_source = flash_program();
        no_source.identity.vin = None;
        let (result, host) = restart_with(&no_source, Some(TARGET_VIN), |_| {});
        assert_eq!(teardown_of(&result), passive);
        assert_eq!(host.sent(), []);
    }

    #[test]
    fn a_job_without_a_vin_reads_nothing_and_stays_passive() {
        let (result, host) = restart_with(&flash_program(), None, |_| {});
        assert_eq!(
            teardown_of(&result),
            passive_gate(restart::PassiveReason::VinNotEstablished)
        );
        assert_eq!(host.sent(), []);
    }

    #[test]
    fn a_hardware_identity_that_differs_or_cannot_be_read_leaves_the_teardown_passive() {
        use restart::PassiveReason;
        // The journal recorded HW01.
        let (result, host) = restart_with(&flash_program(), Some(TARGET_VIN), |host| {
            host.hardware = Some(b"HW02".to_vec());
        });
        assert_eq!(
            teardown_of(&result),
            passive_gate(PassiveReason::HardwareIdentityDiffers)
        );
        assert_eq!(host.sent(), [read_of([0xF1, 0x90]), read_of([0xF1, 0x91])]);

        let (result, _) = restart_with(&flash_program(), Some(TARGET_VIN), |host| {
            host.hardware = None;
        });
        assert_eq!(
            teardown_of(&result),
            passive_gate(PassiveReason::HardwareIdentityNotEstablished)
        );

        // No source declared: nothing is compared.
        let mut no_source = flash_program();
        no_source.identity.hardware_part_number = None;
        let (result, host) = restart_with(&no_source, Some(TARGET_VIN), |_| {});
        assert_eq!(
            teardown_of(&result),
            passive_gate(PassiveReason::HardwareIdentityNotEstablished)
        );
        assert_eq!(host.sent(), [read_of([0xF1, 0x90])]);
    }

    #[test]
    fn a_precondition_that_fails_or_cannot_be_established_leaves_the_teardown_passive() {
        use diag_ir::{PreconditionKind, RuntimeInput};
        use restart::PassiveReason;
        let engine = passive_gate(PassiveReason::Precondition(PreconditionKind::Engine));

        // The engine runs.
        let program = flash_program_with_engine(
            input(RuntimeInput::EngineRunning),
            input(RuntimeInput::EngineRunning),
        );
        let (result, host) = restart_with(&program, Some(TARGET_VIN), |host| {
            host.inputs = crate::inputs::FixedInputs::new().with(
                RuntimeInput::EngineRunning,
                crate::inputs::Reading::Value(1),
            );
        });
        assert_eq!(teardown_of(&result), engine);
        assert_only_reads(&host);

        // The VCI has no source for it.
        let (result, _) = restart_with(&program, Some(TARGET_VIN), |_| {});
        assert_eq!(teardown_of(&result), engine);

        // A value out of range fails at once: the programming-session source, here a field of
        // the VIN request, is not read (the VIN and the hardware identity were read once).
        let program = flash_program_with_engine(
            input(RuntimeInput::EngineRunning),
            diag_ir::Source::EcuService {
                service_id: 1,
                field_id: 3,
            },
        );
        let (result, host) = restart_with(&program, Some(TARGET_VIN), |host| {
            host.inputs = crate::inputs::FixedInputs::new().with(
                RuntimeInput::EngineRunning,
                crate::inputs::Reading::Value(1),
            );
        });
        assert_eq!(teardown_of(&result), engine);
        assert_eq!(host.sent(), [read_of([0xF1, 0x90]), read_of([0xF1, 0x91])]);

        // A source the table lacks cannot be established; no request is sent for it.
        let missing = |field_id| diag_ir::Source::EcuService {
            service_id: 1,
            field_id,
        };
        let program = flash_program_with_engine(missing(8), missing(9));
        let (result, host) = restart_with(&program, Some(TARGET_VIN), |_| {});
        assert_eq!(teardown_of(&result), engine);
        assert_eq!(host.sent(), [read_of([0xF1, 0x90]), read_of([0xF1, 0x91])]);
    }

    /// The ECU may still be in its programming session, so a precondition the default-session
    /// source cannot give is read through the programming-session one.
    #[test]
    fn a_precondition_falls_back_to_the_programming_session_source() {
        use diag_ir::RuntimeInput;
        // The default-session source is not in the table; the other gives an in-range value.
        let program = flash_program_with_engine(
            diag_ir::Source::EcuService {
                service_id: 1,
                field_id: 8,
            },
            input(RuntimeInput::EngineRunning),
        );
        let (result, host) = restart_with(&program, Some(TARGET_VIN), |host| {
            host.inputs = crate::inputs::FixedInputs::new().with(
                RuntimeInput::EngineRunning,
                crate::inputs::Reading::Value(0),
            );
        });
        assert_eq!(teardown_of(&result), restart::Teardown::Reset);
        assert_reads_then_reset(&host);
    }

    /// Every gate passes, with all five preconditions declared: the reset is allowed.
    #[test]
    fn a_restart_whose_gates_all_pass_may_reset() {
        use diag_ir::RuntimeInput as I;
        let mut program = flash_program();
        let declare = |input: I, lower, upper| {
            let source = Some(diag_ir::Source::RuntimeInput(input));
            Some(diag_ir::Precondition {
                satisfied: diag_ir::Satisfied { lower, upper },
                default_session: source,
                programming_session: source,
            })
        };
        let p = &mut program.preconditions;
        p.voltage_mv = declare(I::SupplyVoltageMillivolts, 11_000, 15_000);
        p.external_supply = declare(I::ExternalSupplyConnected, 1, 1);
        p.ignition = declare(I::IgnitionOn, 0, 0);
        p.engine = declare(I::EngineRunning, 0, 0);
        p.vehicle_speed = declare(I::VehicleSpeedKmh, 0, 0);
        program.validate().expect("the fixture is a valid program");
        let fixed = |speed| {
            crate::inputs::FixedInputs::new()
                .with(I::ExternalSupplyConnected, crate::inputs::Reading::Value(1))
                .with(I::IgnitionOn, crate::inputs::Reading::Value(0))
                .with(I::EngineRunning, crate::inputs::Reading::Value(0))
                .with(I::VehicleSpeedKmh, crate::inputs::Reading::Value(speed))
        };
        let (result, host) = restart_with(&program, Some(TARGET_VIN), |host| {
            host.inputs = fixed(0);
        });
        assert_eq!(teardown_of(&result), restart::Teardown::Reset);
        assert_reads_then_reset(&host);

        // The last one in the order fails.
        let (result, _) = restart_with(&program, Some(TARGET_VIN), |host| {
            host.inputs = fixed(5);
        });
        assert_eq!(
            teardown_of(&result),
            passive_gate(restart::PassiveReason::Precondition(
                diag_ir::PreconditionKind::VehicleSpeed
            ))
        );
    }

    #[test]
    fn a_worker_failure_in_a_gate_read_leaves_the_teardown_passive() {
        use diag_ir::{PreconditionKind, RuntimeInput};
        use restart::PassiveReason;
        let (result, _) = restart_with(&flash_program(), Some(TARGET_VIN), |host| {
            host.fail_reads = vec![[0xF1, 0x90]];
        });
        assert_eq!(
            teardown_of(&result),
            passive_gate(PassiveReason::VinNotEstablished)
        );
        let (result, _) = restart_with(&flash_program(), Some(TARGET_VIN), |host| {
            host.fail_reads = vec![[0xF1, 0x91]];
        });
        assert_eq!(
            teardown_of(&result),
            passive_gate(PassiveReason::HardwareIdentityNotEstablished)
        );
        // A precondition read through the ECU, with the programming-session source giving
        // nothing either.
        let ecu = |field_id| diag_ir::Source::EcuService {
            service_id: 1,
            field_id,
        };
        let program = flash_program_with_engine(ecu(2), input(RuntimeInput::EngineRunning));
        let (result, _) = restart_with(&program, Some(TARGET_VIN), |host| {
            host.fail_reads = vec![[0xF1, 0x95]];
        });
        assert_eq!(
            teardown_of(&result),
            passive_gate(PassiveReason::Precondition(PreconditionKind::Engine))
        );
    }

    /// A worker failure on the default-session read fails the gate; the programming-session
    /// source, which would give an in-range value, is not tried (ADR-261 item 5).
    #[test]
    fn a_failed_precondition_read_does_not_fall_back() {
        use diag_ir::{PreconditionKind, RuntimeInput};
        let program = flash_program_with_engine(
            diag_ir::Source::EcuService {
                service_id: 1,
                field_id: 2,
            },
            input(RuntimeInput::EngineRunning),
        );
        let (result, _) = restart_with(&program, Some(TARGET_VIN), |host| {
            host.fail_reads = vec![[0xF1, 0x95]];
            host.inputs = crate::inputs::FixedInputs::new().with(
                RuntimeInput::EngineRunning,
                crate::inputs::Reading::Value(0),
            );
        });
        assert_eq!(
            teardown_of(&result),
            passive_gate(restart::PassiveReason::Precondition(
                PreconditionKind::Engine
            ))
        );
    }

    #[test]
    fn a_vin_source_the_table_lacks_leaves_the_teardown_passive() {
        let mut program = flash_program();
        program.identity.vin = Some(diag_ir::Source::EcuService {
            service_id: 1,
            field_id: 9,
        });
        let (result, host) = restart_with(&program, Some(TARGET_VIN), |_| {});
        assert_eq!(
            teardown_of(&result),
            passive_gate(restart::PassiveReason::VinNotEstablished)
        );
        assert_eq!(host.sent(), []);
    }

    /// A VIN that is not well-formed never aborts the job, whichever side it is on.
    #[test]
    fn a_malformed_vin_is_not_established_and_never_aborts() {
        let passive = passive_gate(restart::PassiveReason::VinNotEstablished);
        for answer in [
            "wdb12345678901234",
            "WDB1234567890123O",
            "WDB 2345678901234",
        ] {
            assert_eq!(answer.len(), 17);
            let (result, host) = restart_with(&flash_program(), Some(TARGET_VIN), |host| {
                host.vin = Some(answer.as_bytes().to_vec());
            });
            assert_eq!(teardown_of(&result), passive, "{answer}");
            assert_eq!(host.sent(), [read_of([0xF1, 0x90])]);
        }
        let (result, host) = restart_with(&flash_program(), Some("SHORT"), |_| {});
        assert_eq!(teardown_of(&result), passive);
        assert_eq!(host.sent(), []);
    }

    // ---------------------------------------------- restart teardown (ADR-229 item 2 step 2b-1)

    /// A first run of `program` that `prepare` makes fail with a host error, then a restart on a
    /// host `prepare_restart` sets up. Gives the result, the host and how long the restart took.
    fn restart_after(
        program: &Program,
        prepare_first: impl FnOnce(&mut FlashHost),
        prepare_restart: impl FnOnce(&mut FlashHost),
    ) -> (Result<VmState, JobError>, FlashHost, Duration) {
        let dir = journal_dir("resume-teardown");
        let result = first_run(program, &dir, JobLimits::default(), prepare_first);
        assert!(matches!(result, Err(JobError::Host { .. })), "{result:?}");
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        prepare_restart(&mut host);
        let started = std::time::Instant::now();
        let result = resume(program, &dir, &mut host, false);
        let elapsed = started.elapsed();
        std::fs::remove_dir_all(&dir).unwrap();
        (result, host, elapsed)
    }

    fn passive(cause: restart::PassiveCause) -> restart::Teardown {
        restart::Teardown::Passive(cause)
    }

    fn session_wait() -> Duration {
        Duration::from_millis(u64::from(SESSION_TIMEOUT_MS + TEARDOWN_MARGIN_MS))
    }

    /// Gates that allow it and a journal that excludes nothing: exactly one ECUReset
    /// (hardReset, no suppress bit), after the gates' reads, and no wait.
    #[test]
    fn the_teardown_sends_one_ecu_reset_when_nothing_rules_it_out() {
        let mut program = flash_program();
        program.flash[0].timing.session_timeout_millis = 60_000;
        let (result, host, elapsed) =
            restart_after(&program, |host| host.lose_routine = Some(0xFF00), |_| {});
        assert_eq!(teardown_of(&result), restart::Teardown::Reset);
        assert_eq!(
            host.sent(),
            [read_of([0xF1, 0x90]), read_of([0xF1, 0x91]), reset_sent()]
        );
        // An accepted reset waits for nothing, however long the session timeout is.
        assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
    }

    #[test]
    fn a_gate_that_fails_makes_the_teardown_passive_without_a_reset() {
        let (result, host, elapsed) = restart_after(
            &flash_program(),
            |host| host.lose_routine = Some(0xFF00),
            |host| host.vin = None,
        );
        assert_eq!(
            teardown_of(&result),
            passive_gate(restart::PassiveReason::VinNotEstablished)
        );
        assert_eq!(host.sent(), [read_of([0xF1, 0x90])]);
        assert!(elapsed >= session_wait(), "{elapsed:?}");
    }

    /// The ECU refuses the reset: passive with the code, the wait is at least the session
    /// timeout plus the margin, and nothing is sent after the refused reset.
    #[test]
    fn a_refused_ecu_reset_makes_the_teardown_passive_and_silent() {
        let (result, host, elapsed) = restart_after(
            &flash_program(),
            |host| host.lose_routine = Some(0xFF00),
            |host| host.reset = ResetAnswer::Refuse(0x22),
        );
        assert_eq!(
            teardown_of(&result),
            passive(restart::PassiveCause::ResetRefused { nrc: 0x22 })
        );
        assert_eq!(
            host.sent(),
            [read_of([0xF1, 0x90]), read_of([0xF1, 0x91]), reset_sent()]
        );
        assert!(elapsed >= session_wait(), "{elapsed:?}");
    }

    /// No answer, a final response pending and a positive answer that is not the reset's are
    /// all an unknown outcome.
    #[test]
    fn an_ecu_reset_with_no_usable_answer_has_an_unknown_outcome() {
        for answer in [
            ResetAnswer::NoAnswer,
            ResetAnswer::Garbled,
            ResetAnswer::Trailing,
            ResetAnswer::Refuse(0x78),
        ] {
            let (result, host, elapsed) = restart_after(
                &flash_program(),
                |host| host.lose_routine = Some(0xFF00),
                |host| host.reset = answer,
            );
            assert_eq!(
                teardown_of(&result),
                passive(restart::PassiveCause::ResetOutcomeUnknown),
                "{answer:?}"
            );
            assert_eq!(host.sent().last(), Some(&reset_sent()), "{answer:?}");
            assert!(elapsed >= session_wait(), "{answer:?}: {elapsed:?}");
        }
    }

    /// The restart's journal says the RequestTransferExit was written ahead and sent without a
    /// response, or sent and the post-transfer steps not finished: no ECUReset, whatever the
    /// gates say, and the passive wait.
    #[test]
    fn a_journaled_transfer_exit_rules_the_reset_out() {
        let (program, _) = program_with_check(diag_ir::RecoveryRequired::Never);
        type Prepare = fn(&mut FlashHost);
        let cases: [(&str, Prepare); 2] = [
            ("the exit has no response", |host| {
                host.lose_service = Some(0x37)
            }),
            ("the post-transfer steps did not finish", |host| {
                host.lose_routine = Some(0xFF01)
            }),
        ];
        for (name, prepare) in cases {
            let (result, host, elapsed) = restart_after(&program, prepare, |_| {});
            assert_eq!(
                teardown_of(&result),
                passive(restart::PassiveCause::TransferExitJournaled),
                "{name}"
            );
            assert_eq!(
                host.sent(),
                [read_of([0xF1, 0x90]), read_of([0xF1, 0x91])],
                "{name}"
            );
            assert!(elapsed >= session_wait(), "{name}: {elapsed:?}");
        }
    }

    /// The post-transfer steps completed before the interruption: the journal reaches the
    /// restart path (`classify` places the point at the plan's end), where the teardown sends no
    /// reset and waits for nothing.
    #[test]
    fn the_completed_path_gets_no_reset_and_no_wait() {
        let (mut program, check) = program_with_check(diag_ir::RecoveryRequired::Never);
        // A routine after the plan's end, whose response is lost.
        program.code.extend([
            Op::PushBytes(0),
            Op::RoutineControl {
                routine: 0xFF02,
                sub: 1,
            },
            Op::Pop,
        ]);
        assert_eq!(program.flash[0].boundaries.post_transfer_end_pc, check + 2);
        program.validate().unwrap();
        // A session timeout that would show if the teardown waited.
        program.flash[0].timing.session_timeout_millis = 60_000;
        let (result, host, elapsed) =
            restart_after(&program, |host| host.lose_routine = Some(0xFF02), |_| {});
        assert_eq!(teardown_of(&result), restart::Teardown::CompletedPath);
        assert_eq!(host.sent(), [read_of([0xF1, 0x90]), read_of([0xF1, 0x91])]);
        assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
    }

    /// A cancel during the passive wait ends the job in `Cancelled` well before the wait would.
    #[test]
    fn a_cancel_during_the_passive_wait_cancels_the_restart() {
        let mut program = flash_program();
        program.flash[0].timing.session_timeout_millis = 60_000;
        let dir = journal_dir("resume-teardown-cancel");
        interrupted(&program, &dir);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        // The VIN is not established, so the teardown is passive.
        host.vin = None;
        let limits = JobLimits {
            wait_poll: Duration::from_millis(2),
            ..JobLimits::default()
        };
        // The cancel comes once the VIN read was sent, so it lands in the passive wait however
        // long the journal's commits before it take.
        let mirror = Arc::new(Mutex::new(Vec::new()));
        host.mirror = Some(Arc::clone(&mirror));
        let canceller = {
            let (cancelled, mirror) = (Arc::clone(&cancelled), Arc::clone(&mirror));
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while mirror.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                std::thread::sleep(Duration::from_millis(10));
                cancelled.store(true, Ordering::Relaxed);
            })
        };
        let started = std::time::Instant::now();
        let result = resume_on(
            &program,
            &mut host,
            limits,
            &cancelled,
            Journal::open(&dir, &job_key()),
            identity_sources(),
            Some(&target()),
            &dir_slot(&dir),
        );
        canceller.join().unwrap();
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(40));
        assert_eq!(host.sent(), [read_of([0xF1, 0x90])]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ------------------------------- default-session confirmation (ADR-229 item 2 step 2b-2)

    /// The teardown and the confirmation a restart ended in. Fails the test on any other result.
    fn confirmed_of(
        result: &Result<VmState, JobError>,
    ) -> (restart::Teardown, restart::Confirmation) {
        match result {
            Err(JobError::OnSiteInterventionRequired(OnSiteReason::RestartOrderUnavailable {
                flash_session: 1,
                teardown,
                confirmed,
            })) => (teardown.clone(), *confirmed),
            other => panic!("{other:?}"),
        }
    }

    /// The teardown of a restart whose ECU could not be confirmed in its default session. Fails
    /// the test on any other result.
    fn not_confirmed_of(result: &Result<VmState, JobError>) -> restart::Teardown {
        match result {
            Err(JobError::OnSiteInterventionRequired(
                OnSiteReason::DefaultSessionNotConfirmed {
                    flash_session: 1,
                    teardown,
                },
            )) => teardown.clone(),
            other => panic!("{other:?}"),
        }
    }

    /// `flash_program` with the post-transfer check and then `tail` after the plan's end.
    fn program_with_tail(tail: impl IntoIterator<Item = Op>) -> Program {
        let (mut program, check) = program_with_check(diag_ir::RecoveryRequired::Never);
        program.code.extend(tail);
        assert_eq!(program.flash[0].boundaries.post_transfer_end_pc, check + 2);
        program.validate().unwrap();
        program
    }

    /// A routine after the plan's end, whose response a first run loses.
    fn completed_path_program() -> Program {
        program_with_tail([
            Op::PushBytes(0),
            Op::RoutineControl {
                routine: 0xFF02,
                sub: 1,
            },
            Op::Pop,
        ])
    }

    /// `program`'s timing for the confirmation, in ms.
    fn confirmation_timing(mut program: Program, startup: u32, window: u32) -> Program {
        program.flash[0].timing.ecu_startup_millis = startup;
        program.flash[0].timing.confirmation_window_millis = window;
        program
    }

    /// The three first reads of F186 that fail a confirmation: a refusal, no answer and another
    /// session.
    fn failed_session_reads() -> [(&'static str, SessionAnswer); 3] {
        [
            ("refused", SessionAnswer::Refuse(0x22)),
            ("no answer", SessionAnswer::NoAnswer),
            ("another session", SessionAnswer::Other(0x03)),
        ]
    }

    #[test]
    fn a_restart_confirms_the_default_session_after_an_accepted_reset() {
        let (result, host, _) = restart_after(
            &flash_program(),
            |host| host.lose_routine = Some(0xFF00),
            |_| {},
        );
        assert_eq!(
            confirmed_of(&result),
            (
                restart::Teardown::Reset,
                restart::Confirmation {
                    after_passive_retry: false
                }
            )
        );
        assert_eq!(host.session_reads.len(), 1);
        assert_eq!(host.sent().last(), Some(&reset_sent()));
    }

    /// A refused ECUReset makes the teardown passive; the ECU is then confirmed, with no
    /// second passive teardown.
    #[test]
    fn a_passive_teardown_is_followed_by_the_confirmation() {
        let (result, host, elapsed) = restart_after(
            &flash_program(),
            |host| host.lose_routine = Some(0xFF00),
            |host| host.reset = ResetAnswer::Refuse(0x22),
        );
        assert_eq!(
            confirmed_of(&result),
            (
                passive(restart::PassiveCause::ResetRefused { nrc: 0x22 }),
                restart::Confirmation {
                    after_passive_retry: false
                }
            )
        );
        assert_eq!(host.session_reads.len(), 1);
        assert!(elapsed >= session_wait(), "{elapsed:?}");
    }

    /// A passive teardown whose confirmation fails ends the job at once: no second passive
    /// teardown and no second attempt.
    #[test]
    fn a_passive_teardown_whose_confirmation_fails_is_not_repeated() {
        for (name, answer) in failed_session_reads() {
            let (result, host, _) = restart_after(
                &flash_program(),
                |host| host.lose_routine = Some(0xFF00),
                |host| {
                    host.vin = None;
                    host.session = answer;
                },
            );
            assert_eq!(
                not_confirmed_of(&result),
                passive_gate(restart::PassiveReason::VinNotEstablished),
                "{name}"
            );
            assert_eq!(host.session_reads.len(), 1, "{name}");
        }
    }

    /// After an accepted reset, a first confirmation that fails gets exactly one passive
    /// teardown and one more attempt; a second that fails ends in on-site intervention.
    #[test]
    fn a_failed_confirmation_after_a_reset_gets_one_passive_teardown_and_one_more_attempt() {
        for (name, answer) in failed_session_reads() {
            let (result, host, elapsed) = restart_after(
                &flash_program(),
                |host| host.lose_routine = Some(0xFF00),
                |host| host.session_script = [answer].into(),
            );
            assert_eq!(
                confirmed_of(&result),
                (
                    restart::Teardown::Reset,
                    restart::Confirmation {
                        after_passive_retry: true
                    }
                ),
                "{name}"
            );
            assert_eq!(host.session_reads.len(), 2, "{name}");
            // The passive wait lies between the two attempts.
            let gap = host.session_reads[1] - host.session_reads[0];
            assert!(gap >= session_wait(), "{name}: {gap:?}");
            assert!(elapsed >= session_wait(), "{name}: {elapsed:?}");

            let (result, host, _) = restart_after(
                &flash_program(),
                |host| host.lose_routine = Some(0xFF00),
                |host| host.session = answer,
            );
            assert_eq!(
                not_confirmed_of(&result),
                restart::Teardown::Reset,
                "{name}"
            );
            assert_eq!(host.session_reads.len(), 2, "{name}");
        }
    }

    /// The window retries the read: a refusal and another session before the default one still
    /// confirm in the first attempt.
    #[test]
    fn the_window_retries_failed_reads() {
        let program = confirmation_timing(flash_program(), 0, 5_000);
        let (result, host, _) = restart_after(
            &program,
            |host| host.lose_routine = Some(0xFF00),
            |host| {
                host.session_script = [
                    SessionAnswer::Refuse(0x22),
                    SessionAnswer::NoAnswer,
                    SessionAnswer::Other(0x03),
                ]
                .into();
            },
        );
        assert_eq!(
            confirmed_of(&result).1,
            restart::Confirmation {
                after_passive_retry: false
            }
        );
        assert_eq!(host.session_reads.len(), 4);
    }

    /// An ECU that does not answer F186 at all ends in on-site intervention after the window.
    #[test]
    fn an_ecu_that_does_not_answer_f186_cannot_be_confirmed() {
        let program = confirmation_timing(flash_program(), 0, 40);
        let (result, host, elapsed) = restart_after(
            &program,
            |host| host.lose_routine = Some(0xFF00),
            |host| host.session = SessionAnswer::NoAnswer,
        );
        assert_eq!(not_confirmed_of(&result), restart::Teardown::Reset);
        // Both attempts read for the window (every 10 ms), with the passive wait between them.
        assert!(
            host.session_reads.len() >= 4,
            "{}",
            host.session_reads.len()
        );
        assert!(
            elapsed >= session_wait() + Duration::from_millis(80),
            "{elapsed:?}"
        );
    }

    /// An ECU that refuses the reset and leaves the confirmation unanswered.
    #[test]
    fn an_ecu_that_refuses_the_reset_and_cannot_be_confirmed_needs_on_site_intervention() {
        for reset in [ResetAnswer::Refuse(0x22), ResetAnswer::NoAnswer] {
            let (result, host, _) = restart_after(
                &flash_program(),
                |host| host.lose_routine = Some(0xFF00),
                |host| {
                    host.reset = reset;
                    host.session = SessionAnswer::Other(0x03);
                },
            );
            assert!(
                matches!(
                    not_confirmed_of(&result),
                    restart::Teardown::Passive(
                        restart::PassiveCause::ResetRefused { .. }
                            | restart::PassiveCause::ResetOutcomeUnknown
                    )
                ),
                "{result:?}"
            );
            assert_eq!(host.session_reads.len(), 1);
        }
    }

    /// On the completed path nothing was reset: a first read that is refused, times out or
    /// reports another session gets the passive teardown and is confirmed on the second.
    fn completed_path_retry(answer: SessionAnswer) {
        let program = completed_path_program();
        let (result, host, elapsed) = restart_after(
            &program,
            |host| host.lose_routine = Some(0xFF02),
            |host| host.session_script = [answer].into(),
        );
        assert_eq!(
            confirmed_of(&result),
            (
                restart::Teardown::CompletedPath,
                restart::Confirmation {
                    after_passive_retry: true
                }
            )
        );
        assert_eq!(host.session_reads.len(), 2);
        assert!(!host.sent().contains(&reset_sent()));
        assert!(elapsed >= session_wait(), "{elapsed:?}");
        let gap = host.session_reads[1] - host.session_reads[0];
        assert!(gap >= session_wait(), "{gap:?}");

        // Still failing after the passive teardown: on-site intervention.
        let (result, host, _) = restart_after(
            &program,
            |host| host.lose_routine = Some(0xFF02),
            |host| host.session = answer,
        );
        assert_eq!(not_confirmed_of(&result), restart::Teardown::CompletedPath);
        assert_eq!(host.session_reads.len(), 2);
    }

    #[test]
    fn a_refused_first_read_on_the_completed_path_is_retried_after_the_passive_teardown() {
        completed_path_retry(SessionAnswer::Refuse(0x22));
    }

    #[test]
    fn an_unanswered_first_read_on_the_completed_path_is_retried_after_the_passive_teardown() {
        completed_path_retry(SessionAnswer::NoAnswer);
    }

    #[test]
    fn a_non_default_first_read_on_the_completed_path_is_retried_after_the_passive_teardown() {
        completed_path_retry(SessionAnswer::Other(0x03));
    }

    /// An interruption right after the procedure's own ECUReset was recorded as complete still
    /// waits the startup time before the first read.
    #[test]
    fn the_startup_time_passes_before_the_first_read_after_a_post_transfer_reset() {
        let program = confirmation_timing(
            program_with_tail([
                Op::PushBytes(0),
                Op::ServiceRequest { service: 0x11 },
                Op::Pop,
            ]),
            80,
            0,
        );
        let dir = journal_dir("resume-startup-wait");
        let result = first_run(&program, &dir, JobLimits::default(), |host| {
            host.lose_service = Some(0x11);
        });
        assert!(matches!(result, Err(JobError::Host { .. })), "{result:?}");
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        let started = std::time::Instant::now();
        let result = resume(&program, &dir, &mut host, false);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            confirmed_of(&result),
            (
                restart::Teardown::CompletedPath,
                restart::Confirmation {
                    after_passive_retry: false
                }
            )
        );
        assert_eq!(host.session_reads.len(), 1);
        let waited = host.session_reads[0] - started;
        assert!(waited >= Duration::from_millis(80), "{waited:?}");
    }

    #[test]
    fn the_journal_setup_debug_output_hides_the_vin() {
        let dir = journal_dir("debug-vin");
        let setup = file_setup(&dir);
        assert!(setup.vin.is_some());
        assert!(!format!("{setup:?}").contains(TARGET_VIN));
        let journal = JobJournal::create(setup).unwrap();
        let state = journal.journal().state();
        assert!(state.facts.target_vin.is_some());
        assert!(!format!("{state:?} {:?}", state.facts).contains(TARGET_VIN));
        drop(journal);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A resume that names another target VIN than the journal recorded, or one where either
    /// side names none, is on-site intervention: nothing is sent and no resume is counted.
    #[test]
    fn a_resume_naming_another_target_vin_sends_nothing_and_counts_nothing() {
        let program = flash_program();
        let cases: [(Option<&str>, Option<&str>); 3] = [
            (Some(TARGET_VIN), Some("WDB99999999999999")),
            (Some(TARGET_VIN), None),
            (None, Some(TARGET_VIN)),
        ];
        for (recorded, named) in cases {
            let dir = journal_dir("resume-vin-differs");
            interrupted_for(&program, &dir, recorded);
            let mut host = FlashHost::new(Rc::new(Cell::new(0)));
            let result = resume_for(&program, &dir, &mut host, false, named);
            assert!(
                matches!(
                    result,
                    Err(JobError::OnSiteInterventionRequired(
                        OnSiteReason::TargetVinDiffers
                    ))
                ),
                "{recorded:?} {named:?}: {result:?}"
            );
            assert_eq!(host.sent(), []);
            assert_eq!(resumes(&dir), 0);
            std::fs::remove_dir_all(&dir).unwrap();
        }
        // The same VIN goes on to the gates.
        let dir = journal_dir("resume-vin-same");
        interrupted(&program, &dir);
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        let result = resume(&program, &dir, &mut host, false);
        assert_eq!(teardown_of(&result), restart::Teardown::Reset);
        assert_eq!(resumes(&dir), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_cancel_during_the_hardware_read_cancels_the_restart() {
        let dir = journal_dir("resume-gates-cancel-hw");
        let program = flash_program();
        interrupted(&program, &dir);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        host.cancel_on_read = Some(([0xF1, 0x91], Arc::clone(&cancelled)));
        let result = resume_on(
            &program,
            &mut host,
            JobLimits::default(),
            &cancelled,
            Journal::open(&dir, &job_key()),
            identity_sources(),
            Some(&target()),
            &dir_slot(&dir),
        );
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
        assert_eq!(host.sent(), [read_of([0xF1, 0x90]), read_of([0xF1, 0x91])]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A cancel during the VIN read is a cancel, whatever the read gave.
    #[test]
    fn a_cancel_during_the_vin_read_cancels_the_restart() {
        let dir = journal_dir("resume-gates-cancel");
        let program = flash_program();
        interrupted(&program, &dir);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        host.cancel_on_read = Some(([0xF1, 0x90], Arc::clone(&cancelled)));
        let result = resume_on(
            &program,
            &mut host,
            JobLimits::default(),
            &cancelled,
            Journal::open(&dir, &job_key()),
            identity_sources(),
            Some(&target()),
            &dir_slot(&dir),
        );
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
        assert_eq!(host.sent(), [read_of([0xF1, 0x90])]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ------------------------------------ promotion to the vehicle lock (ADR-263)

    const POLL: Duration = Duration::from_millis(1);

    /// Guards of another VCI in the lock directory `guard_setup` uses for `dir`, holding the
    /// target vehicle.
    fn other_job_holding_the_vehicle(dir: &std::path::Path) -> JobGuards {
        let mut other = other_vci_guards(dir);
        other
            .take_vehicle(&target(), POLL, &AtomicBool::new(false))
            .unwrap();
        other
    }

    fn other_vci_guards(dir: &std::path::Path) -> JobGuards {
        let setup = crate::guards::GuardSetup {
            vci: "VCI-2".to_owned(),
            ..guard_setup(dir)
        };
        JobGuards::take_vci_only(&setup, POLL, &AtomicBool::new(false)).unwrap()
    }

    /// Whether `take` waits for something another holder has: it is cancelled after a short
    /// delay and ends in `GuardError::Cancelled`. A take that succeeds first gives `false`.
    fn waits_for_holder<T>(
        take: impl FnOnce(&AtomicBool) -> Result<T, crate::guards::GuardError>,
    ) -> bool {
        let flag = Arc::new(AtomicBool::new(false));
        let canceller = {
            let flag = Arc::clone(&flag);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                flag.store(true, Ordering::Relaxed);
            })
        };
        let result = take(&flag);
        canceller.join().unwrap();
        matches!(result, Err(crate::guards::GuardError::Cancelled))
    }

    fn vehicle_is_held(guards: &mut JobGuards) -> bool {
        waits_for_holder(|cancelled| guards.take_vehicle(&target(), POLL, cancelled))
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let start = std::time::Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "the condition did not come true"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A restart on its own thread, recording its requests in `mirror`.
    fn spawn_restart(
        program: &Program,
        dir: &std::path::Path,
        slot: &GuardSlot,
        mirror: &Arc<Mutex<Vec<Sent>>>,
        cancelled: &Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<Result<VmState, JobError>> {
        let (program, dir) = (program.clone(), dir.to_owned());
        let (slot, mirror, cancelled) =
            (Arc::clone(slot), Arc::clone(mirror), Arc::clone(cancelled));
        std::thread::spawn(move || {
            let mut host = FlashHost::new(Rc::new(Cell::new(0)));
            host.mirror = Some(mirror);
            host.voltage = Some(12_600);
            resume_on(
                &program,
                &mut host,
                JobLimits::default(),
                &cancelled,
                Journal::open(&dir, &job_key()),
                identity_sources(),
                Some(&target()),
                &slot,
            )
        })
    }

    /// A restart whose VIN matches leaves the vehicle locked in the guards it returns; another
    /// job cannot take the vehicle until they are dropped.
    #[test]
    fn a_restart_keeps_the_vehicle_lock_in_its_guards() {
        let program = flash_program();
        let dir = journal_dir("promote-keeps");
        interrupted(&program, &dir);
        let slot: GuardSlot = Arc::new(Mutex::new(Some(take_guards(&dir))));
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        let result = resume_in(&program, &dir, &mut host, false, Some(TARGET_VIN), &slot);
        assert_eq!(teardown_of(&result), restart::Teardown::Reset);
        let guards = slot.lock().unwrap().take().unwrap();
        assert!(guards.holds_vehicle());
        assert!(guards.holds_slot());

        let mut other = other_vci_guards(&dir);
        assert!(vehicle_is_held(&mut other));
        drop(guards);
        assert!(!vehicle_is_held(&mut other));
        assert!(other.holds_vehicle());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// While another job holds the vehicle, the promotion waits right after the VIN read: no
    /// request follows it and the guard slot is not locked. The job goes on once the vehicle is
    /// free.
    #[test]
    fn the_promotion_waits_for_the_vehicle_with_nothing_more_sent() {
        let program = flash_program();
        let dir = journal_dir("promote-waits");
        interrupted(&program, &dir);
        let other = other_job_holding_the_vehicle(&dir);
        let slot: GuardSlot = Arc::new(Mutex::new(Some(take_guards(&dir))));
        let mirror = Arc::new(Mutex::new(Vec::new()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let job = spawn_restart(&program, &dir, &slot, &mirror, &cancelled);

        // The guards are out of the slot, which can be locked: the job waits for the vehicle.
        wait_until(|| slot.try_lock().is_ok_and(|slot| slot.is_none()));
        assert_eq!(*mirror.lock().unwrap(), [read_of([0xF1, 0x90])]);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!job.is_finished());
        assert_eq!(*mirror.lock().unwrap(), [read_of([0xF1, 0x90])]);

        drop(other);
        let result = job.join().unwrap();
        assert_eq!(teardown_of(&result), restart::Teardown::Reset);
        assert_eq!(
            *mirror.lock().unwrap(),
            [read_of([0xF1, 0x90]), read_of([0xF1, 0x91]), reset_sent()]
        );
        let guards = slot.lock().unwrap().take().unwrap();
        assert!(guards.holds_vehicle());
        assert!(guards.holds_slot());
        drop(guards);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A cancel during that wait ends the job in `Cancelled`; the guards come back with their
    /// VCI lock and slot and without the vehicle.
    #[test]
    fn a_cancel_during_the_promotion_returns_the_guards_without_the_vehicle() {
        let program = flash_program();
        let dir = journal_dir("promote-cancel");
        interrupted(&program, &dir);
        let _other = other_job_holding_the_vehicle(&dir);
        let slot: GuardSlot = Arc::new(Mutex::new(Some(take_guards(&dir))));
        let mirror = Arc::new(Mutex::new(Vec::new()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let job = spawn_restart(&program, &dir, &slot, &mirror, &cancelled);

        wait_until(|| slot.try_lock().is_ok_and(|slot| slot.is_none()));
        cancelled.store(true, Ordering::Relaxed);
        let result = job.join().unwrap();
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
        assert_eq!(*mirror.lock().unwrap(), [read_of([0xF1, 0x90])]);

        let guards = slot.lock().unwrap().take().unwrap();
        assert!(!guards.holds_vehicle());
        assert!(guards.holds_slot());
        // Their VCI lock is still held.
        let setup = guard_setup(&dir);
        assert!(waits_for_holder(|cancelled| {
            JobGuards::take_vci_only(&setup, POLL, cancelled)
        }));
        drop(guards);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A restart that does not establish the vehicle, or finds another, takes no vehicle lock.
    #[test]
    fn a_restart_without_a_matching_vin_takes_no_vehicle_lock() {
        let cases = [
            (
                "mismatch",
                Some(TARGET_VIN),
                Some(b"WDB99999999999999".to_vec()),
                true,
            ),
            ("unread", Some(TARGET_VIN), None, true),
            (
                "no-target",
                None,
                Some(TARGET_VIN.as_bytes().to_vec()),
                true,
            ),
            (
                "no-source",
                Some(TARGET_VIN),
                Some(TARGET_VIN.as_bytes().to_vec()),
                false,
            ),
        ];
        for (tag, vin, answer, has_source) in cases {
            let mut program = flash_program();
            if !has_source {
                program.identity.vin = None;
            }
            let dir = journal_dir("promote-none");
            interrupted_for(&program, &dir, vin);
            let slot: GuardSlot = Arc::new(Mutex::new(Some(take_guards(&dir))));
            let mut host = FlashHost::new(Rc::new(Cell::new(0)));
            host.vin = answer;
            let _ = resume_in(&program, &dir, &mut host, false, vin, &slot);
            let guards = slot.lock().unwrap().take().unwrap();
            assert!(!guards.holds_vehicle(), "{tag}");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// Guards that hold the job's vehicle already go on without waiting, run after run.
    #[test]
    fn guards_that_hold_the_vehicle_resume_without_waiting() {
        let program = flash_program();
        let dir = journal_dir("promote-held");
        interrupted(&program, &dir);
        let mut guards = take_guards(&dir);
        guards
            .take_vehicle(&target(), POLL, &AtomicBool::new(false))
            .unwrap();
        let slot: GuardSlot = Arc::new(Mutex::new(Some(guards)));
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        let result = resume_in(&program, &dir, &mut host, false, Some(TARGET_VIN), &slot);
        assert_eq!(teardown_of(&result), restart::Teardown::Reset);
        assert!(slot.lock().unwrap().as_ref().unwrap().holds_vehicle());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A vehicle lock file that cannot be used ends the job with nothing sent after the VIN
    /// read, and no VIN in the error.
    #[test]
    fn a_failing_vehicle_lock_ends_the_restart() {
        let program = flash_program();
        let dir = journal_dir("promote-fails");
        interrupted(&program, &dir);
        let guards = take_guards(&dir);
        // The bucket's file is a directory.
        std::fs::create_dir(crate::guards::vehicle_path(
            &dir.join("locks"),
            crate::guards::vehicle_bucket(&target()),
        ))
        .unwrap();
        let slot: GuardSlot = Arc::new(Mutex::new(Some(guards)));
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        let result = resume_in(&program, &dir, &mut host, false, Some(TARGET_VIN), &slot);
        let Err(error @ JobError::VehicleLock(_)) = &result else {
            panic!("{result:?}");
        };
        let text = format!("{error} {error:?}");
        let bucket = format!("{:03x}", crate::guards::vehicle_bucket(&target()));
        assert!(!text.contains("WDB"), "{text}");
        assert!(!text.contains("vehicle-"), "{text}");
        assert!(!text.contains(&bucket), "{text}");
        assert_eq!(host.sent(), [read_of([0xF1, 0x90])]);
        let guards = slot.lock().unwrap().take().unwrap();
        assert!(!guards.holds_vehicle());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An empty slot is an error, not a panic.
    #[test]
    fn a_restart_with_an_empty_guard_slot_ends_in_an_error() {
        let program = flash_program();
        let dir = journal_dir("promote-empty");
        interrupted(&program, &dir);
        let slot: GuardSlot = Arc::new(Mutex::new(None));
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        let result = resume_in(&program, &dir, &mut host, false, Some(TARGET_VIN), &slot);
        assert!(matches!(result, Err(JobError::GuardsMissing)), "{result:?}");
        assert_eq!(host.sent(), [read_of([0xF1, 0x90])]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A second resume of a job whose journal another run holds ends with nothing sent and no
    /// on-site verdict: the other run goes on with the job (ADR-255).
    #[test]
    fn a_resume_of_a_job_another_run_holds_sends_nothing() {
        let program = flash_program_with_voltage();
        let dir = journal_dir("resume-in-use");
        interrupted(&program, &dir);
        let held = Journal::open(&dir, &job_key()).unwrap();
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        host.voltage = Some(12_600);
        let result = resume(&program, &dir, &mut host, false);
        assert!(
            matches!(result, Err(JobError::Journal(JournalError::InUse))),
            "{result:?}"
        );
        assert_eq!(host.sent(), []);
        assert_eq!(host.voltage_reads, 0);
        drop(held);
        assert_eq!(resumes(&dir), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A resume count that cannot be committed ends the job with nothing sent.
    #[test]
    fn a_failed_resume_commit_ends_the_job_with_nothing_sent() {
        let program = flash_program();
        let interrupted_journal = |fail_at| {
            let commits = Rc::new(Cell::new(0));
            let mut host = FlashHost::new(Rc::clone(&commits));
            host.lose_routine = Some(0xFF00);
            let store = CountingStore {
                commits: Rc::clone(&commits),
                fail_at,
            };
            let mut inner = crate::journal::Journal::on_store(store, job_key());
            inner.commit_target_vin(&target()).unwrap();
            let mut journal = JobJournal::new(inner, identity_sources());
            let result = run_on(
                &program,
                &mut host,
                JobLimits::default(),
                &AtomicBool::new(false),
                Some(&mut journal),
            );
            assert!(matches!(result, Err(JobError::Host { pc: ERASE, .. })));
            (journal.into_journal(), commits.get())
        };
        let (_, first_run_commits) = interrupted_journal(None);
        // The same run again, on a store that fails the next commit: the resume's.
        let (journal, commits) = interrupted_journal(Some(first_run_commits + 1));
        assert_eq!(commits, first_run_commits);
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        let result = resume_on(
            &program,
            &mut host,
            JobLimits::default(),
            &AtomicBool::new(false),
            Ok(journal),
            identity_sources(),
            Some(&target()),
            &vci_only_slot("resume"),
        );
        assert!(matches!(result, Err(JobError::Journal(_))), "{result:?}");
        assert_eq!(host.sent(), []);
    }

    /// A job stopped before its erase starts again on the same journal, its records coming
    /// after the old ones.
    #[test]
    fn a_plain_start_goes_on_with_the_existing_journal() {
        let program = flash_program();
        let dir = journal_dir("resume-plain");
        // Stops on arriving at the erase, after the identity reads and the step into the entry.
        let limits = JobLimits {
            max_steps: u64::from(ERASE),
            ..JobLimits::default()
        };
        let result = first_run(&program, &dir, limits, |_| {});
        assert!(matches!(result, Err(JobError::StepLimit(_))), "{result:?}");
        let before = Journal::read(&dir, &job_key()).unwrap();
        assert!(before.facts.transfer.is_none());
        let first_step = restart::next_steps(&before);
        assert!(first_step > 0);

        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        let state = resume(&program, &dir, &mut host, false).expect("the program runs");
        assert_eq!(state.steps, first_step + program.code.len() as u64);
        assert!(host.sent().contains(&Sent::Routine(0xFF00)));
        let after = Journal::read(&dir, &job_key()).unwrap();
        let transfer = after.facts.transfer.as_ref().expect("a transfer");
        assert!(transfer.started_at.steps >= first_step);
        assert!(transfer.exit.as_ref().is_some_and(|exit| exit.complete));
        assert_eq!(after.facts.resume_count(crate::journal::StageId(7)), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A program with a plan whose journal is missing or does not read back needs on-site
    /// intervention, with nothing sent; one without a plan and without a journal runs.
    #[test]
    fn a_missing_or_unreadable_journal_sends_nothing() {
        let program = flash_program();
        let dir = journal_dir("resume-missing");
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        let result = resume(&program, &dir, &mut host, false);
        assert!(
            matches!(
                result,
                Err(JobError::OnSiteInterventionRequired(
                    OnSiteReason::MissingJournal
                ))
            ),
            "{result:?}"
        );
        assert_eq!(host.sent(), []);

        drop(Journal::create(&dir, &job_key(), None).unwrap());
        std::fs::write(journal_file(&dir), b"not a journal").unwrap();
        let result = resume(&program, &dir, &mut host, false);
        assert!(
            matches!(
                result,
                Err(JobError::OnSiteInterventionRequired(
                    OnSiteReason::UnreadableJournal(_)
                ))
            ),
            "{result:?}"
        );
        assert_eq!(host.sent(), []);
        std::fs::remove_dir_all(&dir).unwrap();

        let plain = program_without_plan();
        let dir = journal_dir("resume-no-plan");
        let mut host = FlashHost::new(Rc::new(Cell::new(0)));
        resume(&plain, &dir, &mut host, false).expect("the program runs");
        assert_eq!(host.sent().len(), 2);
        // Only the guards' lock directory: no journal was created.
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["locks"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn program_without_plan() -> Program {
        program(two_requests())
    }

    /// A client for a port nothing listens on: any RPC fails, so a test that gets past the
    /// checks of `run_program` ends in `JobError::Link`.
    fn unreachable_client() -> WorkerClient {
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
        vci_service_interface::vci_service_client::VciServiceClient::with_interceptor(
            channel,
            worker_host::client::BearerAuth::new([0; 32], "test"),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_program_with_a_refused_request_never_reaches_the_worker() {
        let code = vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::ServiceRequest { service: 0x2E },
        ];
        let (result, _guards) = run_guarded(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(code.clone()),
            JobLimits::default(),
            Permission::ReadOnly,
            None,
            vci_only_guards("refused"),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(JobError::Refused {
                    pc: 2,
                    source: HostError::NotAllowed(0x2E)
                })
            ),
            "{result:?}"
        );
        // Without the check, the same job reaches the worker.
        let (result, _guards) = run_guarded(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(two_requests()[..2].to_vec()),
            JobLimits::default(),
            Permission::ReadOnly,
            None,
            vci_only_guards("reads"),
        )
        .await;
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
    }

    #[cfg(debug_assertions)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_program_with_a_write_reaches_the_worker_under_the_simulator_ceiling() {
        let code = vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x2E },
            Op::RoutineControl {
                routine: 0xFF01,
                sub: 1,
            },
        ];
        let (result, _guards) = run_guarded(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(code.clone()),
            JobLimits::default(),
            Permission::Simulator,
            None,
            full_guards("sim-ceiling"),
        )
        .await;
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
        // The public entry point uses the build's ceiling, which a debug build sets to it.
        let (result, _guards) = run_program(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(code),
            JobLimits::default(),
            full_guards("sim-public"),
        )
        .await;
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
    }

    /// A program that writes needs the reprogramming slot: on VCI-only guards it ends before
    /// anything opens, and the guards come back.
    #[cfg(debug_assertions)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_writing_program_on_vci_only_guards_is_refused() {
        let (result, guards) = run_program(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            flash_program(),
            JobLimits::default(),
            vci_only_guards("no-slot"),
        )
        .await;
        assert!(
            matches!(result, Err(JobError::NoReprogrammingSlot)),
            "{result:?}"
        );
        assert!(!guards.holds_slot());
        // With the slot, the same program gets past the check and reaches the link.
        let (result, guards) = run_program(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            flash_program(),
            JobLimits::default(),
            full_guards("with-slot"),
        )
        .await;
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
        assert!(guards.holds_slot());
    }

    /// A program that only reads runs on VCI-only guards.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_only_program_runs_on_vci_only_guards() {
        let (result, guards) = run_program(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(two_requests()[..2].to_vec()),
            JobLimits::default(),
            vci_only_guards("read-only"),
        )
        .await;
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
        assert!(!guards.holds_slot());
    }

    #[test]
    fn the_link_permission_refuses_what_the_ceiling_let_through() {
        let write = program(vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::RoutineControl { routine: 1, sub: 1 },
        ]);
        assert!(matches!(
            refuse_beyond(&write, Permission::ReadOnly),
            Err(JobError::Refused {
                pc: 2,
                source: HostError::NotAllowed(0x31)
            })
        ));
        assert!(
            refuse_beyond(&program(two_requests()[..2].to_vec()), Permission::ReadOnly).is_ok()
        );
        #[cfg(debug_assertions)]
        assert!(refuse_beyond(&write, Permission::Simulator).is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_job_cancelled_before_it_starts_never_reaches_the_worker() {
        let handle = Handle::current();
        let slot = vci_only_slot("cancelled");
        let result = tokio::task::spawn_blocking(move || {
            let config = LinkConfig::iso15765(0x7E0, 0x7E8);
            let program = program(two_requests());
            // Without the check, opening the link fails against the unreachable client.
            run_job(
                handle,
                unreachable_client(),
                &config,
                &program,
                JobLimits::default(),
                &AtomicBool::new(true),
                None,
                slot,
            )
        })
        .await
        .unwrap();
        assert!(matches!(result, Err(JobError::Cancelled)), "{result:?}");
    }

    #[test]
    fn dropping_the_job_future_sets_the_cancel_flag() {
        let guard = CancelOnDrop(Arc::new(AtomicBool::new(false)));
        let flag = Arc::clone(&guard.0);
        assert!(!flag.load(Ordering::Relaxed));
        drop(guard);
        assert!(flag.load(Ordering::Relaxed));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_current_thread_runtime_is_refused() {
        // Refused before the client is used.
        let client = unreachable_client();
        let (result, _guards) = run_program(
            client,
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(Vec::new()),
            JobLimits::default(),
            vci_only_guards("current-thread"),
        )
        .await;
        assert!(matches!(result, Err(JobError::CurrentThreadRuntime)));
    }

    // ------------------------------------------------------------ closing the link (ADR-258)

    /// A worker that answers the RPCs a link needs and records them. `fail_open` makes the link's
    /// connect fail and the module's disconnect too, as a VCI unplugged during the open does.
    /// `fail_disconnect` makes the
    /// logical link's disconnect fail, as a VCI that was unplugged does.
    struct FakeWorker {
        fail_disconnect: bool,
        fail_open: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl FakeWorker {
        fn note(&self, rpc: &'static str) {
            self.calls.lock().unwrap().push(rpc);
        }
    }

    fn unimplemented<T>() -> Result<tonic::Response<T>, tonic::Status> {
        Err(tonic::Status::unimplemented("not part of the fake"))
    }

    #[tonic::codegen::async_trait]
    impl vci_service_interface::vci_service_server::VciService for FakeWorker {
        async fn get_module_ids(
            &self,
            _: tonic::Request<vci_service_interface::GetModuleIdsRequest>,
        ) -> Result<tonic::Response<vci_service_interface::ModuleIdsResponse>, tonic::Status>
        {
            self.note("GetModuleIds");
            Ok(tonic::Response::new(
                vci_service_interface::ModuleIdsResponse {
                    module_id_list: Some(vci_service_interface::ModuleItem {
                        module_data: vec![vci_service_interface::ModuleData {
                            module_handle: Some(ModuleHandle { module_handle: 1 }),
                            ..Default::default()
                        }],
                    }),
                },
            ))
        }
        async fn module_connect(
            &self,
            _: tonic::Request<vci_service_interface::ModuleConnectRequest>,
        ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
            self.note("ModuleConnect");
            Ok(tonic::Response::new(Default::default()))
        }
        async fn module_disconnect(
            &self,
            _: tonic::Request<vci_service_interface::ModuleDisconnectRequest>,
        ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
            self.note("ModuleDisconnect");
            if self.fail_open {
                return Err(tonic::Status::failed_precondition("device not connected"));
            }
            Ok(tonic::Response::new(Default::default()))
        }
        async fn get_version(
            &self,
            _: tonic::Request<vci_service_interface::GetVersionRequest>,
        ) -> Result<tonic::Response<vci_service_interface::VersionResponse>, tonic::Status>
        {
            // The link stays read-only.
            unimplemented()
        }
        async fn get_timestamp(
            &self,
            _: tonic::Request<vci_service_interface::GetTimestampRequest>,
        ) -> Result<tonic::Response<vci_service_interface::TimestampResponse>, tonic::Status>
        {
            unimplemented()
        }
        async fn get_resource_status(
            &self,
            _: tonic::Request<vci_service_interface::GetResourceStatusRequest>,
        ) -> Result<tonic::Response<vci_service_interface::ResourceStatusResponse>, tonic::Status>
        {
            unimplemented()
        }
        async fn get_resource_ids(
            &self,
            _: tonic::Request<vci_service_interface::GetResourceIdsRequest>,
        ) -> Result<tonic::Response<vci_service_interface::ResourceIdsResponse>, tonic::Status>
        {
            Ok(tonic::Response::new(
                vci_service_interface::ResourceIdsResponse {
                    resource_id_list: Some(vci_service_interface::ResourceIdItem {
                        resource_id_data_array: vec![vci_service_interface::ResourceIdItemData {
                            module_handle: None,
                            resource_id_array: vec![1],
                        }],
                    }),
                },
            ))
        }
        async fn get_conflicting_resources(
            &self,
            _: tonic::Request<vci_service_interface::GetConflictingResourcesRequest>,
        ) -> Result<
            tonic::Response<vci_service_interface::ConflictingResourcesResponse>,
            tonic::Status,
        > {
            unimplemented()
        }
        async fn create_com_logical_link(
            &self,
            _: tonic::Request<vci_service_interface::CreateComLogicalLinkRequest>,
        ) -> Result<tonic::Response<vci_service_interface::ComLogicalLinkResponse>, tonic::Status>
        {
            Ok(tonic::Response::new(
                vci_service_interface::ComLogicalLinkResponse {
                    cll_handle: Some(ComLogicalLinkHandle {
                        module_handle: 1,
                        cll_handle: 1,
                    }),
                },
            ))
        }
        async fn destroy_com_logical_link(
            &self,
            _: tonic::Request<vci_service_interface::DestroyComLogicalLinkRequest>,
        ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
            self.note("DestroyComLogicalLink");
            Ok(tonic::Response::new(Default::default()))
        }
        async fn connect_com_logical_link(
            &self,
            _: tonic::Request<vci_service_interface::ConnectComLogicalLinkRequest>,
        ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
            self.note("ConnectComLogicalLink");
            if self.fail_open {
                return Err(tonic::Status::failed_precondition("device not connected"));
            }
            Ok(tonic::Response::new(Default::default()))
        }
        async fn disconnect_com_logical_link(
            &self,
            _: tonic::Request<vci_service_interface::DisconnectComLogicalLinkRequest>,
        ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
            self.note("DisconnectComLogicalLink");
            if self.fail_disconnect {
                return Err(tonic::Status::failed_precondition("device not connected"));
            }
            Ok(tonic::Response::new(Default::default()))
        }
        async fn lock_resource(
            &self,
            _: tonic::Request<vci_service_interface::LockResourceRequest>,
        ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
            unimplemented()
        }
        async fn unlock_resource(
            &self,
            _: tonic::Request<vci_service_interface::UnlockResourceRequest>,
        ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
            unimplemented()
        }
        async fn get_com_param(
            &self,
            _: tonic::Request<vci_service_interface::GetComParamRequest>,
        ) -> Result<tonic::Response<vci_service_interface::ComParamResponse>, tonic::Status>
        {
            unimplemented()
        }
        async fn set_com_param(
            &self,
            _: tonic::Request<vci_service_interface::SetComParamRequest>,
        ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
            Ok(tonic::Response::new(Default::default()))
        }
        async fn start_com_primitive(
            &self,
            _: tonic::Request<vci_service_interface::StartComPrimitiveRequest>,
        ) -> Result<tonic::Response<vci_service_interface::ComPrimitiveResponse>, tonic::Status>
        {
            self.note("StartComPrimitive");
            unimplemented()
        }
        async fn cancel_com_primitive(
            &self,
            _: tonic::Request<vci_service_interface::CancelComPrimitiveRequest>,
        ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
            unimplemented()
        }
        async fn get_status(
            &self,
            _: tonic::Request<vci_service_interface::GetStatusRequest>,
        ) -> Result<tonic::Response<vci_service_interface::StatusResponse>, tonic::Status> {
            unimplemented()
        }
        async fn get_event_item(
            &self,
            _: tonic::Request<vci_service_interface::GetEventItemRequest>,
        ) -> Result<tonic::Response<vci_service_interface::EventItemResponse>, tonic::Status>
        {
            unimplemented()
        }
        type SubscribeEventStream = tonic::codegen::tokio_stream::Pending<
            Result<vci_service_interface::EventNotification, tonic::Status>,
        >;
        async fn subscribe_event(
            &self,
            _: tonic::Request<vci_service_interface::SubscribeEventRequest>,
        ) -> Result<tonic::Response<Self::SubscribeEventStream>, tonic::Status> {
            Ok(tonic::Response::new(tonic::codegen::tokio_stream::pending()))
        }
        async fn io_ctl(
            &self,
            _: tonic::Request<vci_service_interface::IoCtlRequest>,
        ) -> Result<tonic::Response<vci_service_interface::IoCtlResponse>, tonic::Status> {
            unimplemented()
        }
        async fn get_object_id(
            &self,
            _: tonic::Request<vci_service_interface::GetObjectIdRequest>,
        ) -> Result<tonic::Response<vci_service_interface::ObjectIdResponse>, tonic::Status>
        {
            Ok(tonic::Response::new(
                vci_service_interface::ObjectIdResponse { pdu_object_id: 1 },
            ))
        }
        async fn get_unique_resp_id_table(
            &self,
            _: tonic::Request<vci_service_interface::GetUniqueRespIdTableRequest>,
        ) -> Result<tonic::Response<vci_service_interface::UniqueRespIdTableResponse>, tonic::Status>
        {
            unimplemented()
        }
        async fn set_unique_resp_id_table(
            &self,
            _: tonic::Request<vci_service_interface::SetUniqueRespIdTableRequest>,
        ) -> Result<tonic::Response<vci_service_interface::Response>, tonic::Status> {
            Ok(tonic::Response::new(Default::default()))
        }
    }

    /// Serves a [`FakeWorker`] on a loopback port; returns a client for it and the log of the
    /// RPCs it recorded.
    async fn fake_worker(fail_disconnect: bool) -> (WorkerClient, Arc<Mutex<Vec<&'static str>>>) {
        fake_worker_with(fail_disconnect, false).await
    }

    async fn fake_worker_with(
        fail_disconnect: bool,
        fail_open: bool,
    ) -> (WorkerClient, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let service = FakeWorker {
            fail_disconnect,
            fail_open,
            calls: Arc::clone(&calls),
        };
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let incoming = tonic::codegen::tokio_stream::wrappers::TcpListenerStream::new(listener);
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(
                    vci_service_interface::vci_service_server::VciServiceServer::new(service),
                )
                .serve_with_incoming(incoming),
        );
        let channel = tonic::transport::Endpoint::from_shared(format!("http://127.0.0.1:{port}"))
            .unwrap()
            .connect_lazy();
        let client = vci_service_interface::vci_service_client::VciServiceClient::with_interceptor(
            channel,
            worker_host::client::BearerAuth::new([0; 32], "test"),
        );
        (client, calls)
    }

    /// A link whose disconnect fails is not confirmed closed: the job's result stands, and the
    /// guards it returns say so and keep their locks.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_close_marks_the_guards_and_keeps_the_result() {
        let (client, calls) = fake_worker(true).await;
        let (result, mut guards) = run_program(
            client,
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(Vec::new()),
            JobLimits::default(),
            vci_only_guards("close-fails"),
        )
        .await;
        result.expect("the program has nothing to fail");
        assert!(guards.link_unconfirmed());
        let calls = calls.lock().unwrap().clone();
        assert!(calls.contains(&"DisconnectComLogicalLink"), "{calls:?}");
        // Every step of the close was tried.
        assert!(calls.contains(&"ModuleDisconnect"), "{calls:?}");
        guards.worker_gone();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_confirmed_close_leaves_the_guards_unmarked() {
        let (client, calls) = fake_worker(false).await;
        let (result, guards) = run_program(
            client,
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(Vec::new()),
            JobLimits::default(),
            vci_only_guards("close-works"),
        )
        .await;
        result.expect("the program has nothing to fail");
        assert!(!guards.link_unconfirmed());
        let calls = calls.lock().unwrap().clone();
        assert!(calls.contains(&"ModuleDisconnect"), "{calls:?}");
    }

    /// The close also runs, and also marks, when the job fails: its own error is what the run
    /// returns.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_close_after_a_failed_job_keeps_the_jobs_error() {
        let (client, _calls) = fake_worker(true).await;
        let (result, mut guards) = run_program(
            client,
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(two_requests()[..2].to_vec()),
            JobLimits::default(),
            vci_only_guards("job-and-close-fail"),
        )
        .await;
        assert!(
            matches!(result, Err(JobError::Host { pc: 1, .. })),
            "{result:?}"
        );
        assert!(guards.link_unconfirmed());
        guards.worker_gone();
    }

    /// An open that fails after the module connected, and cannot disconnect it again, leaves
    /// the module possibly connected: the guards come back marked (ADR-258).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_open_that_cannot_clean_up_marks_the_guards() {
        let (client, calls) = fake_worker_with(false, true).await;
        let (result, mut guards) = run_program(
            client,
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(Vec::new()),
            JobLimits::default(),
            vci_only_guards("open-fails"),
        )
        .await;
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
        assert!(guards.link_unconfirmed());
        let calls = calls.lock().unwrap().clone();
        assert!(calls.contains(&"ConnectComLogicalLink"), "{calls:?}");
        assert!(calls.contains(&"ModuleDisconnect"), "{calls:?}");
        guards.worker_gone();
    }

    /// Guards whose link was not confirmed closed are refused before anything opens, until
    /// `worker_gone`.
    #[tokio::test(flavor = "multi_thread")]
    async fn unconfirmed_guards_are_refused_until_the_worker_is_gone() {
        let (client, calls) = fake_worker(false).await;
        let mut guards = vci_only_guards("refused-unconfirmed");
        guards.mark_link_unconfirmed();
        let (result, mut guards) = run_program(
            client.clone(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(Vec::new()),
            JobLimits::default(),
            guards,
        )
        .await;
        assert!(
            matches!(result, Err(JobError::LinkUnconfirmed)),
            "{result:?}"
        );
        assert!(guards.link_unconfirmed());
        assert!(calls.lock().unwrap().is_empty(), "nothing was sent");
        guards.worker_gone();
        let (result, guards) = run_program(
            client,
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(Vec::new()),
            JobLimits::default(),
            guards,
        )
        .await;
        result.expect("runs once the worker is gone");
        assert!(!guards.link_unconfirmed());
    }
}

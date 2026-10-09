//! Runs a procedure on a link (ADR-235).
//!
//! No server and no section policy. A host error ends the job; the VM state then still points
//! at the failed primitive (ADR-233 item 1). [`run_program_journaled`] also writes the
//! write-job journal at a flash recovery plan's boundaries (`journaling`, ADR-252).

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use diag_ir::{
    DiagHost, IdentityKind, Program, StepError, StepOutcome, Vm, VmError, VmState, WaitingOn,
};
use tokio::runtime::{Handle, RuntimeFlavor};
use worker_host::client::WorkerClient;

use crate::host::{HostError, Timings, TransferProgress, WorkerHost};
use crate::inputs::RuntimeInputs;
use crate::journal::{JournalError, StepRef, Store};
use crate::journaling::{JobJournal, JournalSetup};
use crate::link::{self, LinkConfig};
use crate::policy::{self, Permission};

/// Bounds of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    /// Steps after which the job is stopped, against a procedure that never ends.
    pub max_steps: u64,
    /// Pause between polls of a `Wait` instruction; at least 1 ms is used.
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
    #[error("the job thread panicked")]
    Panicked,
}

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
/// logged, since it does not change the results. Dropping the returned future cancels the job:
/// the call or primitive in flight finishes, no further instruction runs, and the link is
/// closed. That happens after the future is gone, so a caller that dropped it must not hand
/// the worker to another job yet (ADR-235 consequences).
///
/// The program is checked against [`policy::build_ceiling`] before anything opens and against
/// the link's own permission once the VCI is known (ADR-247).
pub async fn run_program(
    client: WorkerClient,
    config: &LinkConfig,
    program: Program,
    limits: JobLimits,
) -> Result<VmState, JobError> {
    run_program_within(
        client,
        config,
        program,
        limits,
        policy::build_ceiling(),
        None,
    )
    .await
}

/// [`run_program`] that also writes the write-job journal for a program with a flash recovery
/// plan (ADR-244, ADR-252): the journal of `journal.key` is created in `journal.dir` once the
/// link is open and the policy allows the program, before anything is sent to the ECU, and the
/// runner commits to it at the plan's boundaries (`journaling`). A commit that fails ends the
/// job before the request it guards is sent. A program without a plan keeps no journal and
/// creates no file.
pub async fn run_program_journaled(
    client: WorkerClient,
    config: &LinkConfig,
    program: Program,
    limits: JobLimits,
    journal: JournalSetup,
) -> Result<VmState, JobError> {
    run_program_within(
        client,
        config,
        program,
        limits,
        policy::build_ceiling(),
        Some(journal),
    )
    .await
}

/// [`run_program`] with the build's ceiling given, so tests can run under a lower one.
async fn run_program_within(
    client: WorkerClient,
    config: &LinkConfig,
    program: Program,
    limits: JobLimits,
    ceiling: Permission,
    journal: Option<JournalSetup>,
) -> Result<VmState, JobError> {
    let handle = Handle::current();
    if handle.runtime_flavor() == RuntimeFlavor::CurrentThread {
        return Err(JobError::CurrentThreadRuntime);
    }
    check_program(&program, ceiling)?;
    let config = config.clone();

    let cancel = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let cancelled = Arc::clone(&cancel.0);
    tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            run_job(
                handle, client, &config, &program, limits, &cancelled, journal,
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
fn run_job(
    handle: Handle,
    mut client: WorkerClient,
    config: &LinkConfig,
    program: &Program,
    limits: JobLimits,
    cancelled: &AtomicBool,
    journal: Option<JournalSetup>,
) -> Result<VmState, JobError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    let timings = Timings::for_link(config);
    let link = handle
        .block_on(link::open(&mut client, config, timings.unary))
        .map_err(JobError::Link)?;
    // The VCI is known now. Nothing has been sent to the ECU yet, so a program that needs more
    // than this link allows is refused with no effect on it.
    if let Err(error) = refuse_beyond(program, link.permission) {
        close_logged(&handle, &mut client, link, timings.unary);
        return Err(error);
    }
    // Still before anything is sent: a job that cannot keep its journal sends nothing. A link
    // that fails to open, or a program the link refuses, leaves no journal behind.
    let mut journal = match journal {
        Some(setup) if !program.flash.is_empty() => match JobJournal::create(setup) {
            Ok(journal) => Some(journal),
            Err(error) => {
                close_logged(&handle, &mut client, link, timings.unary);
                return Err(error.into());
            }
        },
        _ => None,
    };
    let mut host = WorkerHost::new(handle.clone(), client, link, timings);
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        run_on(program, &mut host, limits, cancelled, journal.as_mut())
    }))
    .unwrap_or(Err(JobError::Panicked));
    let (mut client, link) = host.into_parts();
    close_logged(&handle, &mut client, link, timings.unary);
    result
}

/// Closes the link; the job's result stands whether or not that works, so a failure is only
/// logged.
fn close_logged(handle: &Handle, client: &mut WorkerClient, link: link::Link, deadline: Duration) {
    // Closing on a runtime that is shutting down can panic.
    let closed = std::panic::catch_unwind(AssertUnwindSafe(|| {
        handle.block_on(link::close(client, link, deadline))
    }));
    match closed {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "could not close the link"),
        Err(_) => tracing::warn!("closing the link panicked"),
    }
}

/// Sets the flag when the job's future is dropped.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
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
    mut journal: Option<&mut JobJournal<S>>,
) -> Result<VmState, JobError>
where
    H: DiagHost<Error = HostError> + RuntimeInputs + TransferProgress,
    S: Store,
{
    let wait_poll = limits.wait_poll.max(Duration::from_millis(1));
    let mut vm = Vm::new(program);
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
            }
        }

        fn sent(&self) -> Vec<Sent> {
            self.log.iter().map(|(sent, _)| sent.clone()).collect()
        }
    }

    impl RuntimeInputs for FlashHost {
        fn read(&mut self, _: diag_ir::RuntimeInput) -> Result<crate::inputs::Reading, HostError> {
            Ok(crate::inputs::Reading::CannotBeEstablished)
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
            if let Some((did, flag)) = &self.cancel_on_read
                && service == 0x22
                && payload == did
            {
                flag.store(true, Ordering::Relaxed);
            }
            match (service, payload) {
                (0x22, [0xF1, 0x91]) => Ok([&[0x62, 0xF1, 0x91][..], b"HW01"].concat()),
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
            timing: diag_ir::RecoveryTiming {
                session_timeout_millis: 5000,
                teardown_margin_millis: 0,
                ecu_startup_millis: 0,
                confirmation_window_millis: 0,
            },
            version_read_retries: 0,
            no_application: None,
        });
        program.validate().expect("the fixture is a valid program");
        program
    }

    fn identity_sources() -> crate::inputs::ServiceSources {
        let field = |field_id, did: u8| crate::inputs::ServiceField {
            service_id: 1,
            field_id,
            request: vec![0x22, 0xF1, did],
            offset: 2,
            length: 4,
            encoding: crate::inputs::Encoding::Ascii,
        };
        crate::inputs::ServiceSources::new(vec![field(1, 0x91), field(2, 0x95)]).unwrap()
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
        };
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
                        Some(setup),
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

    #[test]
    fn a_program_without_a_plan_keeps_no_journal() {
        let dir =
            std::env::temp_dir().join(format!("ngr-runner-no-journal-{}", std::process::id()));
        let setup = JournalSetup {
            dir: dir.clone(),
            key: job_key(),
            sources: identity_sources(),
        };
        // Fails when the link opens, before the journal would be created: no file appears.
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
                        Some(setup),
                    )
                })
                .await
                .unwrap()
            });
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
        assert!(!dir.exists());
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
        let result = run_program_within(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(code.clone()),
            JobLimits::default(),
            Permission::ReadOnly,
            None,
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
        let result = run_program_within(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(two_requests()[..2].to_vec()),
            JobLimits::default(),
            Permission::ReadOnly,
            None,
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
        let result = run_program_within(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(code.clone()),
            JobLimits::default(),
            Permission::Simulator,
            None,
        )
        .await;
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
        // The public entry point uses the build's ceiling, which a debug build sets to it.
        let result = run_program(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(code),
            JobLimits::default(),
        )
        .await;
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
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
        assert!(matches!(
            run_program(
                client,
                &LinkConfig::iso15765(0x7E0, 0x7E8),
                program(Vec::new()),
                JobLimits::default()
            )
            .await,
            Err(JobError::CurrentThreadRuntime)
        ));
    }
}

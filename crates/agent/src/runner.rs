//! Runs a procedure on a link (ADR-235).
//!
//! The minimal runner: no journal, no server, no section policy. A host error ends the job;
//! the VM state then still points at the failed primitive (ADR-233 item 1), which a journaling
//! runner will use to decide whether to repeat it.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use diag_ir::{DiagHost, Program, StepError, StepOutcome, Vm, VmError, VmState, WaitingOn};
use tokio::runtime::{Handle, RuntimeFlavor};
use worker_host::client::WorkerClient;

use crate::host::{HostError, Timings, WorkerHost};
use crate::link::{self, LinkConfig};
use crate::policy;

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
    #[error("the job thread panicked")]
    Panicked,
}

/// Opens a link with `config`, runs `program` on it to the end and closes the link. Returns
/// the final VM state; the procedure's results are on its stack.
///
/// Must be called on a multi-threaded runtime: the VM runs on a blocking thread that blocks on
/// the runtime for every primitive. That thread opens the link, runs the program and closes the
/// link, so no future holding worker resources is ever dropped halfway. The link is closed
/// however the job ends, also when the procedure panics; a failure to close it is logged, since
/// it does not change the results. Dropping the returned future cancels the job: the call or
/// primitive in flight finishes, no further instruction runs, and the link is closed.
pub async fn run_program(
    client: WorkerClient,
    config: &LinkConfig,
    program: Program,
    limits: JobLimits,
) -> Result<VmState, JobError> {
    let handle = Handle::current();
    if handle.runtime_flavor() == RuntimeFlavor::CurrentThread {
        return Err(JobError::CurrentThreadRuntime);
    }
    // A program this VM cannot run must not reach the bus.
    Vm::new(&program)
        .check_state(&program)
        .map_err(|source| JobError::Vm { pc: 0, source })?;
    policy::check_program(&program).map_err(|(pc, source)| JobError::Refused { pc, source })?;
    let config = config.clone();

    let cancel = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let cancelled = Arc::clone(&cancel.0);
    tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            run_job(handle, client, &config, &program, limits, &cancelled)
        }))
        .unwrap_or(Err(JobError::Panicked))
    })
    .await
    .unwrap_or(Err(JobError::Panicked))
}

/// The whole job, on the blocking thread: open, run, close.
fn run_job(
    handle: Handle,
    mut client: WorkerClient,
    config: &LinkConfig,
    program: &Program,
    limits: JobLimits,
    cancelled: &AtomicBool,
) -> Result<VmState, JobError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    let timings = Timings::for_link(config);
    let link = handle
        .block_on(link::open(&mut client, config, timings.unary))
        .map_err(JobError::Link)?;
    let mut host = WorkerHost::new(handle.clone(), client, link, timings);
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        run_on(program, &mut host, limits, cancelled)
    }))
    .unwrap_or(Err(JobError::Panicked));
    let (mut client, link) = host.into_parts();
    // Closing on a runtime that is shutting down can panic; the job's result stands.
    let closed = std::panic::catch_unwind(AssertUnwindSafe(|| {
        handle.block_on(link::close(&mut client, link, timings.unary))
    }));
    match closed {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "could not close the link"),
        Err(_) => tracing::warn!("closing the link panicked"),
    }
    result
}

/// Sets the flag when the job's future is dropped.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The step loop, on the blocking thread.
fn run_on<H: DiagHost<Error = HostError>>(
    program: &Program,
    host: &mut H,
    limits: JobLimits,
    cancelled: &AtomicBool,
) -> Result<VmState, JobError> {
    let wait_poll = limits.wait_poll.max(Duration::from_millis(1));
    let mut vm = Vm::new(program);
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(JobError::Cancelled);
        }
        if vm.state.steps >= limits.max_steps {
            return Err(JobError::StepLimit(vm.state.steps));
        }
        let pc = vm.state.pc;
        match vm.step(program, host) {
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

    fn program(code: Vec<Op>) -> Program {
        Program {
            schema_version: IR_SCHEMA_VERSION,
            code,
            constants: vec![vec![0x01]],
            sections: Vec::new(),
            source_map: Vec::new(),
        }
    }

    fn run(code: Vec<Op>, max_steps: u64, host: &mut FakeHost) -> Result<VmState, JobError> {
        let limits = JobLimits {
            max_steps,
            ..JobLimits::default()
        };
        run_on(&program(code), host, limits, &AtomicBool::new(false))
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
        run_on(&program, &mut host, limits, &AtomicBool::new(false)).unwrap();
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
            run_on(
                &program(two_requests()),
                &mut host,
                JobLimits::default(),
                &flag
            ),
            Err(JobError::Cancelled)
        ));
        assert_eq!(host.requests, 1);
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
        let result = run_program(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(code),
            JobLimits::default(),
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
        let result = run_program(
            unreachable_client(),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            program(two_requests()[..2].to_vec()),
            JobLimits::default(),
        )
        .await;
        assert!(matches!(result, Err(JobError::Link(_))), "{result:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_job_dropped_at_once_stops_before_the_worker() {
        let config = LinkConfig::iso15765(0x7E0, 0x7E8);
        let job = run_program(
            unreachable_client(),
            &config,
            program(two_requests()[..2].to_vec()),
            JobLimits::default(),
        );
        // Polled once, so the blocking thread is spawned, then dropped.
        assert!(tokio::time::timeout(Duration::ZERO, job).await.is_err());
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

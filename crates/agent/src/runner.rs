//! Runs a procedure on a link (ADR-235).
//!
//! The minimal runner: no journal, no server, no section policy. A host error ends the job;
//! the VM state then still points at the failed primitive (ADR-233 item 1), which a journaling
//! runner will use to decide whether to repeat it.

use std::time::Duration;

use diag_ir::{Program, StepError, StepOutcome, Vm, VmError, VmState, WaitingOn};
use tokio::runtime::{Handle, RuntimeFlavor};
use worker_host::client::WorkerClient;

use crate::host::{HostError, Timings, WorkerHost};
use crate::link::{self, LinkConfig};

/// Bounds of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    /// Steps after which the job is stopped, against a procedure that never ends.
    pub max_steps: u64,
    /// Pause between polls of a `Wait` instruction.
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
    #[error("the job thread panicked")]
    Panicked,
}

/// Opens a link with `config`, runs `program` on it to the end and closes the link. Returns
/// the final VM state; the procedure's results are on its stack.
///
/// Must be called on a multi-threaded runtime: the VM runs on a blocking thread that blocks on
/// the runtime for every primitive.
pub async fn run_program(
    mut client: WorkerClient,
    config: &LinkConfig,
    program: Program,
    limits: JobLimits,
) -> Result<VmState, JobError> {
    let handle = Handle::current();
    if handle.runtime_flavor() == RuntimeFlavor::CurrentThread {
        return Err(JobError::CurrentThreadRuntime);
    }
    let timings = Timings::for_link(config);
    let link = link::open(&mut client, config, timings.unary)
        .await
        .map_err(JobError::Link)?;
    let mut host = WorkerHost::new(handle, client, link, timings);

    let (result, host) = tokio::task::spawn_blocking(move || {
        let result = run_on(&program, &mut host, limits);
        (result, host)
    })
    .await
    .map_err(|_| JobError::Panicked)?;

    let (mut client, link) = host.into_parts();
    let closed = link::close(&mut client, &link, timings.unary).await;
    let state = result?;
    closed.map_err(JobError::Link)?;
    Ok(state)
}

/// The step loop, on the blocking thread.
fn run_on(
    program: &Program,
    host: &mut WorkerHost,
    limits: JobLimits,
) -> Result<VmState, JobError> {
    let mut vm = Vm::new(program);
    loop {
        if vm.state.steps >= limits.max_steps {
            return Err(JobError::StepLimit(vm.state.steps));
        }
        let pc = vm.state.pc;
        match vm.step(program, host) {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => return Ok(vm.state),
            Ok(StepOutcome::Waiting(WaitingOn::Timer)) => std::thread::sleep(limits.wait_poll),
            Ok(StepOutcome::Waiting(on)) => return Err(JobError::Unanswerable(on)),
            Err(StepError::Vm(source)) => return Err(JobError::Vm { pc, source }),
            Err(StepError::Host(source)) => return Err(JobError::Host { pc, source }),
        }
    }
}

//! Agent (design 3.3): runs IR procedures against a worker.
//!
//! - [`launch`]: launching the worker that matches a VCI library's ABI (7.3)
//! - [`link`]: the communication link a job runs on, set up through the worker's D-PDU API
//!   before the procedure starts
//! - [`host`]: [`diag_ir::DiagHost`] on top of the worker gRPC client
//! - [`inputs`]: the runtime inputs and service fields a restart reads (ADR-229, ADR-245)
//! - [`journal`]: the write-job journal a restart resumes from (ADR-244)
//! - [`restart`]: how a job whose journal exists goes on (ADR-253, ADR-255)
//! - [`guards`]: the per-VCI lock, the reprogramming slot and the per-vehicle lock (ADR-256)
//! - [`policy`]: which requests the runner may send (read-only, except on the simulator in a debug build)
//! - [`runner`]: runs a [`diag_ir::Program`] to its end on a link
//!
//! The contract between the VM and the worker (what `ServiceRequest` sends, what comes back,
//! deadlines, the sync/async bridge) is ADR-235.

pub mod guards;
pub mod host;
pub mod inputs;
pub mod journal;
mod journaling;
pub mod launch;
pub mod link;
pub mod policy;
pub mod restart;
pub mod runner;

pub use host::{HostError, Timings, WorkerHost};
pub use journaling::JournalSetup;
pub use link::{Link, LinkConfig};
pub use runner::{
    JobError, JobLimits, RestartGuards, check_program, resume_program_journaled, run_program,
    run_program_journaled,
};

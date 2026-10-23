//! Agent (design 3.3): runs IR procedures against a worker.
//!
//! - [`launch`]: launching the worker that matches a VCI library's ABI (7.3)
//! - [`artifact`]: the flash artifact a job's `FlashBlock` reads its blocks from (ADR-336)
//! - [`client`]: the worker client as a trait, so the hardware harness can guard a job's calls (ADR-334)
//! - [`comm_log`]: the log of a job's communication, kept beside its journal (design 8.11, ADR-346,
//!   ADR-347)
//! - [`link`]: the communication link a job runs on, set up through the worker's D-PDU API
//!   before the procedure starts
//! - [`host`]: [`diag_ir::DiagHost`] on top of the worker gRPC client
//! - [`inputs`]: the runtime inputs and service fields a restart reads (ADR-229, ADR-245)
//! - [`journal`]: the write-job journal a restart resumes from (ADR-244)
//! - [`restart`]: how a job whose journal exists goes on (ADR-253, ADR-255)
//! - [`guards`]: the per-VCI lock, the reprogramming slot and the per-vehicle lock (ADR-256,
//!   ADR-262)
//! - [`policy`]: which requests the runner may send (read-only, except on the simulator in a debug build)
//! - [`runner`]: runs a [`diag_ir::Program`] to its end on a link
//! - [`document`]: the variant's document a job's program decodes responses with (ADR-316)
//! - [`send_receive`]: what the generic send-receive's answer is matched against (ADR-321, ADR-322)
//!
//! The contract between the VM and the worker (what `ServiceRequest` sends, what comes back,
//! deadlines, the sync/async bridge) is ADR-235.

pub mod artifact;
pub mod client;
pub mod comm_log;
pub mod document;
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
pub mod send_receive;

pub use artifact::Artifact;
pub use client::WorkerApi;
pub use comm_log::{CommLog, CommLogIncomplete, CommLogSetup, CommLogStatus};
pub use document::JobDocument;
pub use host::{HostError, Timings, WorkerHost};
pub use journal::Vin;
pub use journaling::JournalSetup;
pub use link::{Link, LinkConfig};
pub use runner::{
    Job, JobError, JobLimits, JobRun, check_job, check_program, resume_program_journaled,
    run_program, run_program_journaled,
};

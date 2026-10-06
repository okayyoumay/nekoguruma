//! Agent (design 3.3): runs IR procedures against a worker.
//!
//! - [`link`]: the communication link a job runs on, set up through the worker's D-PDU API
//!   before the procedure starts
//! - [`host`]: [`diag_ir::DiagHost`] on top of the worker gRPC client
//! - [`policy`]: which requests the runner may send (read-only for now)
//! - [`runner`]: runs a [`diag_ir::Program`] to its end on a link
//!
//! The contract between the VM and the worker (what `ServiceRequest` sends, what comes back,
//! deadlines, the sync/async bridge) is ADR-235.

pub mod host;
pub mod link;
pub mod policy;
pub mod runner;

pub use host::{HostError, Timings, WorkerHost};
pub use link::{Link, LinkConfig};
pub use runner::{JobError, JobLimits, run_program};

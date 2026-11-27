//! The shell a job runs its VCI in (design 5.3, 8.2.6, 8.8.1, ADR-397, ADR-511 item 9): finding
//! the VCI's API by its name, taking its locks, launching its worker, running the job on it and
//! stopping the worker.
//!
//! [`run_on_vci`] is the one function `ngr-agent run`, `ngr-agent identify` and the intake call.
//! The wait for the locks takes a cancel flag, so a caller that must stop waiting (a start
//! deadline, a stop of the agent) ends it with [`VciShellError::Cancelled`]. [`VciLookup`] maps
//! the VCI name an instruction carries to what the shell needs; [`RegisteredVcis`] reads it from
//! the registration definitions the workers resolve names with, and tests give a lookup of
//! their own.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use worker_host::service::{LaunchOptions, OperatingMode, ServiceError, Stopped, WorkerLayout};

use crate::comm_log::{CommLogStatus, VciFacts};
use crate::guards::{GuardError, GuardSetup, JobGuards};
use crate::launch::{
    Api, LaunchError, Registered, RegisteredError, RelaunchingWorker, launch_worker, registered_api,
};
use crate::runner::JobError;

/// How often a job waiting for its guards tries again.
const GUARD_POLL: Duration = Duration::from_millis(10);

/// How long a stopping worker may take to close its link before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Why the shell could not run a job, or find the VCI to run it on.
#[derive(Debug, thiserror::Error)]
pub enum VciShellError {
    /// No registration definition names the VCI, so the job has no link to open.
    #[error("no registration definition names the VCI {vci:?}")]
    UnknownVci { vci: String },
    /// Both a J2534 registration definition and a D-PDU API root file name the VCI, so its API
    /// is not decided by its name.
    #[error("the VCI {vci:?} is named by both a J2534 and a D-PDU API registration definition")]
    AmbiguousVci { vci: String },
    /// The registration definitions could not be read, so whether they name the VCI is unknown.
    /// The intake answers it as for [`VciShellError::UnknownVci`]: `preconditionFailed` with
    /// `linkNotOpened`.
    #[error("cannot look the VCI {vci:?} up: {source}")]
    Lookup {
        vci: String,
        #[source]
        source: RegisteredError,
    },
    /// The cancel flag was set while the shell waited for the job's locks.
    #[error("the wait for the job's locks was cancelled")]
    Cancelled,
    #[error("cannot take the job's guards: {0}")]
    Guards(#[source] GuardError),
    #[error("taking the job's guards failed: {0}")]
    GuardsTask(#[source] tokio::task::JoinError),
    /// The worker did not launch, or its client did not connect.
    #[error(transparent)]
    Launch(#[from] LaunchError),
}

/// What the shell needs to reach a VCI that its name does not give.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VciRoute {
    /// The API the VCI's library implements, which decides the worker.
    pub api: Api,
    /// The directory of worker builds (design 7.3).
    pub workers: PathBuf,
    /// The device's lock directory. The lock files are created in it under the VCI's name.
    pub locks: PathBuf,
}

/// Finds the [`VciRoute`] of the VCI an instruction names. Tests replace it.
pub trait VciLookup {
    /// The route of the VCI called `vci`, or [`VciShellError::UnknownVci`] when the definitions
    /// the lookup reads do not name it.
    fn find(&self, vci: &str) -> Result<VciRoute, VciShellError>;
}

/// The lookup of the agent: a VCI is named by a J2534 registration definition or a D-PDU API
/// root file entry (design 7.1.1), the same names the worker resolves, and its API is the one
/// whose definitions name it. The worker builds and the lock directory are the device's.
#[derive(Debug, Clone)]
pub struct RegisteredVcis {
    pub workers: PathBuf,
    pub locks: PathBuf,
}

impl RegisteredVcis {
    /// The lookup with the worker builds and the lock directory the agent's own installation has:
    /// the `workers` and `locks` directories next to the running executable.
    pub fn beside_executable() -> std::io::Result<Self> {
        Ok(Self {
            workers: beside_executable("workers")?,
            locks: beside_executable("locks")?,
        })
    }
}

/// The directory `name` next to the running executable.
pub fn beside_executable(name: &str) -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| {
        std::io::Error::other("the running executable has no directory to look beside")
    })?;
    Ok(dir.join(name))
}

impl VciLookup for RegisteredVcis {
    fn find(&self, vci: &str) -> Result<VciRoute, VciShellError> {
        let registered = registered_api(vci).map_err(|source| VciShellError::Lookup {
            vci: vci.to_owned(),
            source,
        })?;
        let api = match registered {
            Registered::Only(api) => api,
            Registered::Neither => {
                return Err(VciShellError::UnknownVci {
                    vci: vci.to_owned(),
                });
            }
            Registered::Both => {
                return Err(VciShellError::AmbiguousVci {
                    vci: vci.to_owned(),
                });
            }
        };
        Ok(VciRoute {
            api,
            workers: self.workers.clone(),
            locks: self.locks.clone(),
        })
    }
}

/// What a job needs to reach its VCI.
#[derive(Debug, Clone)]
pub struct VciShell {
    /// The VCI's name, as the device's definitions and its locks name it.
    pub vci: String,
    pub mode: OperatingMode,
    pub route: VciRoute,
}

/// What a job gives back to the shell: its result, the guards it was given and how its
/// communication log ended.
pub type JobOutcome<T> = (Result<T, JobError>, JobGuards, CommLogStatus);

/// How the shell ended the job's worker.
#[derive(Debug)]
pub enum WorkerEnd {
    /// The worker exited when it was asked to stop.
    Exited,
    /// The worker did not exit in time and was killed.
    Killed,
    /// The stop failed after the job had confirmed its link closed, so the VCI is free.
    NotClean(ServiceError),
    /// The stop failed with the job's link unconfirmed: the worker may still hold the VCI, and
    /// the guards stay locked until this process exits.
    MayStillHold(ServiceError),
}

/// A job that ran in the shell.
#[derive(Debug)]
pub struct ShellRun<T> {
    /// The job's result.
    pub result: Result<T, JobError>,
    /// How the job's communication log ended.
    pub comm_log: CommLogStatus,
    pub worker_end: WorkerEnd,
}

/// Sets a cancel flag when dropped while armed.
struct CancelOnDrop {
    flag: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(true, Ordering::Relaxed);
        }
    }
}

/// The shell every job runs in (ADR-397): the per-VCI lock (and the reprogramming slot for a job
/// that `writes`, design 8.8.1), the worker launch, the worker's source with its slot, the
/// worker's stop and the guards' release. `job` runs on what the shell sets up: the worker's
/// source, the VCI's facts and the guards; the communication log's setup is the caller's, which
/// hands it to the job. Returns the job's result and how it ended.
///
/// `cancel` ends the wait for the locks: a flag set while the shell waits gives
/// [`VciShellError::Cancelled`], with no worker launched. A flag set while the worker launches
/// stops that worker and gives the same error, without running the job. Dropping the returned
/// future during the wait sets the flag, so the wait ends. Once the job has started the flag does
/// not concern the shell, whose job takes its own cancel.
pub async fn run_on_vci<T, Fut>(
    shell: VciShell,
    writes: bool,
    cancel: Arc<AtomicBool>,
    job: impl FnOnce(RelaunchingWorker, VciFacts, JobGuards) -> Fut,
) -> Result<ShellRun<T>, VciShellError>
where
    Fut: Future<Output = JobOutcome<T>>,
{
    let VciShell { vci, mode, route } = shell;
    let VciRoute {
        api,
        workers,
        locks,
    } = route;
    // The per-VCI lock, and the reprogramming slot for a program that writes (design 8.8.1). The
    // wait polls a file lock, so it runs off the runtime's threads.
    let guard_setup = GuardSetup {
        dir: locks,
        vci: vci.clone(),
    };
    // A future dropped mid-wait (a `select!`, a timeout, a stop) leaves the blocking task
    // polling; the guard sets the flag so the task ends instead of taking locks nobody holds.
    let mut cancel_on_drop = CancelOnDrop {
        flag: Arc::clone(&cancel),
        armed: true,
    };
    let wait_cancel = Arc::clone(&cancel);
    let waited = tokio::task::spawn_blocking(move || {
        if writes {
            JobGuards::take(&guard_setup, GUARD_POLL, &wait_cancel)
        } else {
            JobGuards::take_vci_only(&guard_setup, GUARD_POLL, &wait_cancel)
        }
    })
    .await;
    cancel_on_drop.armed = false;
    let guards = waited
        .map_err(VciShellError::GuardsTask)?
        .map_err(|error| match error {
            GuardError::Cancelled => VciShellError::Cancelled,
            error => VciShellError::Guards(error),
        })?;
    let worker = launch_worker(
        api,
        &vci,
        mode,
        &WorkerLayout { root: workers },
        &LaunchOptions::default(),
        None,
    )
    .await?;
    // A cancel set while the worker launched: stop it and give the locks back without a job.
    if cancel.load(Ordering::Relaxed) {
        // The job never ran, so no link was opened: the guards are not marked, and a failed
        // stop leaves nothing for them to keep.
        if let Err(error) = worker.process.stop(STOP_GRACE).await {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "the worker of a cancelled launch did not stop cleanly"
            );
        }
        drop(guards);
        return Err(VciShellError::Cancelled);
    }
    let facts = VciFacts {
        api,
        long_size: worker.long_size,
    };
    // The run owns the worker's process through its source, which starts a new worker if the
    // current one exits (ADR-394); the process in the slot is the one to stop at the end.
    let source = RelaunchingWorker::new(worker.recipe, worker.process, worker.client);
    let slot = source.slot();
    let (result, mut guards, comm_log) = job(source, facts, guards).await;
    // The job has closed its link whatever the result, unless its guards say otherwise
    // (ADR-258). A stop that reaped the worker means the worker no longer holds the link; a
    // vendor device server may still hold the device a while, which the next open detects.
    let process = slot
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .expect("the run leaves its worker's process in the slot");
    let worker_end = match process.stop(STOP_GRACE).await {
        Ok(Stopped::Exited) => {
            guards.worker_gone();
            WorkerEnd::Exited
        }
        Ok(Stopped::Killed) => {
            guards.worker_gone();
            WorkerEnd::Killed
        }
        // The worker's state is unknown: with an unconfirmed link, the guards stay locked when
        // dropped, until this process exits.
        Err(error) if guards.link_unconfirmed() => WorkerEnd::MayStillHold(error),
        Err(error) => WorkerEnd::NotClean(error),
    };
    // Released only now: either the worker was reaped, or its stop failed after a confirmed
    // close, so its link is closed either way.
    drop(guards);
    Ok(ShellRun {
        result,
        comm_log,
        worker_end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(locks: PathBuf) -> VciRoute {
        VciRoute {
            api: Api::J2534,
            workers: PathBuf::from("no-workers"),
            locks,
        }
    }

    /// A VCI that no definition on a test machine names is unknown, not a launch failure.
    #[test]
    fn the_agents_lookup_refuses_a_vci_no_definition_names() {
        let lookup = RegisteredVcis {
            workers: PathBuf::from("workers"),
            locks: PathBuf::from("locks"),
        };
        let refused = lookup.find("ngr-test-no-such-vci-4f1c9a");
        assert!(
            matches!(&refused, Err(VciShellError::UnknownVci { vci }) if vci == "ngr-test-no-such-vci-4f1c9a"),
            "{refused:?}"
        );
    }

    /// The job of a test whose shell must not reach it.
    async fn never() -> JobOutcome<()> {
        unreachable!("the shell ended before the job started")
    }

    fn shell(locks: PathBuf) -> VciShell {
        VciShell {
            vci: "vci-a".to_owned(),
            mode: OperatingMode::Device,
            route: route(locks),
        }
    }

    /// Another job holds the VCI's lock; setting the cancel flag ends the wait with its own
    /// error, and no job runs and no worker is launched (the workers directory does not exist).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancel_ends_the_wait_for_the_locks() {
        let dir = tempfile::tempdir().unwrap();
        let locks = dir.path().join("locks");
        let holder = JobGuards::take_vci_only(
            &GuardSetup {
                dir: locks.clone(),
                vci: "vci-a".to_owned(),
            },
            Duration::from_millis(1),
            &AtomicBool::new(false),
        )
        .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let waiting = tokio::spawn(run_on_vci(
            shell(locks.clone()),
            false,
            Arc::clone(&cancel),
            |_source, _facts, _guards| never(),
        ));
        // The wait is still going while the holder holds the lock and nothing cancels it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiting.is_finished());
        cancel.store(true, Ordering::Relaxed);
        let ended = tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("the wait ends when it is cancelled")
            .unwrap();
        assert!(
            matches!(ended, Err(VciShellError::Cancelled)),
            "{:?}",
            ended.map(|run| run.worker_end)
        );
        drop(holder);
    }

    /// A future dropped while it waits (a timeout, a `select!`, a stop) sets the flag, so the
    /// blocking wait ends while the lock is still held, and takes nothing once it is released.
    #[test]
    fn a_dropped_wait_ends_its_thread_and_takes_no_lock() {
        let dir = tempfile::tempdir().unwrap();
        let locks = dir.path().join("locks");
        let setup = GuardSetup {
            dir: locks.clone(),
            vci: "vci-a".to_owned(),
        };
        let holder =
            JobGuards::take_vci_only(&setup, Duration::from_millis(1), &AtomicBool::new(false))
                .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let timed_out = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_millis(100),
                run_on_vci(
                    shell(locks),
                    false,
                    Arc::clone(&cancel),
                    |_source, _facts, _guards| never(),
                ),
            )
            .await
        });
        // Shutting the runtime down waits for blocking threads, up to the limit: a wait that
        // kept polling the held lock would use all of it. (Asserting first would panic into a
        // runtime drop that waits for that thread forever.)
        let started = std::time::Instant::now();
        runtime.shutdown_timeout(Duration::from_secs(10));
        let shutdown = started.elapsed();
        assert!(timed_out.is_err(), "the wait outlasts the timeout");
        assert!(cancel.load(Ordering::Relaxed), "dropping sets the flag");
        assert!(
            shutdown < Duration::from_secs(5),
            "the blocking wait kept polling: {shutdown:?}"
        );
        drop(holder);
        // Nothing holds the lock: no phantom taker kept it.
        let again = JobGuards::try_take(&setup).unwrap();
        assert!(again.is_some(), "the lock is free");
    }

    /// A worker that cannot launch is its own error, after the locks were taken and released.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_worker_that_does_not_launch_is_a_launch_error() {
        let dir = tempfile::tempdir().unwrap();
        let locks = dir.path().join("locks");
        let ended = run_on_vci(
            shell(locks.clone()),
            false,
            Arc::new(AtomicBool::new(false)),
            |_source, _facts, _guards| never(),
        )
        .await;
        assert!(
            matches!(ended, Err(VciShellError::Launch(_))),
            "{:?}",
            ended.map(|run| run.worker_end)
        );
        // The lock is free again.
        JobGuards::take_vci_only(
            &GuardSetup {
                dir: locks,
                vci: "vci-a".to_owned(),
            },
            Duration::from_millis(1),
            &AtomicBool::new(false),
        )
        .expect("the failed launch released the lock");
    }
}

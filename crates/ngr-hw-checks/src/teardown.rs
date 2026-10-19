//! The teardown of a hardware check (design 13.5 "Cleanup", ADR-333 item 5).
//!
//! The harness registers a step as it acquires each resource: before it switches power on, when
//! it opens a device, when it connects a link. [`Teardown::run`] runs the registered steps in
//! reverse order and takes each one as it goes, so a step runs at most once however often `run`
//! is called, and [`Drop`] runs what is left, so the steps run when a check panics too. A step
//! that fails or panics does not stop the others; [`Teardown::run`] returns what failed, and the
//! run's [`crate::report::RunGuard`] turns a failure into a failed check.
//!
//! An async step is driven on a detached OS thread with its own current-thread runtime, and the
//! caller waits for it with a timeout, so `run` can be called from anywhere (blocking on a runtime
//! from inside a runtime's worker thread panics, and a thread of its own does not) and a step
//! that blocks inside `poll` cannot hold the teardown up: it is left behind and the rest runs.

use std::any::Any;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// How long an async step may take before it is given up on, unless set otherwise.
pub const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(30);

type SyncStep = Box<dyn FnOnce() -> Result<(), String> + Send>;
type AsyncStep = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

enum Action {
    Sync(SyncStep),
    Async(AsyncStep),
}

type StepTimeout = Box<dyn Fn() -> Duration + Send>;

struct Step {
    name: String,
    action: Action,
    /// The time this async step may take, instead of the teardown's.
    timeout: Option<StepTimeout>,
}

/// A step that failed, panicked or timed out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFailure {
    pub name: String,
    pub reason: String,
}

impl std::fmt::Display for StepFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name, self.reason)
    }
}

/// The steps a check has registered so far.
pub struct Teardown {
    steps: Vec<Step>,
    timeout: Duration,
    /// The run has finished its teardown: a handle refuses further registrations.
    closed: bool,
}

impl Default for Teardown {
    fn default() -> Self {
        Self::new()
    }
}

impl Teardown {
    pub fn new() -> Self {
        Self {
            steps: Vec::new(),
            timeout: DEFAULT_STEP_TIMEOUT,
            closed: false,
        }
    }

    /// Sets the time an async step may take.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Registers a step. Register it as soon as the resource it releases is held.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        step: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) {
        self.steps.push(Step {
            name: name.into(),
            action: Action::Sync(Box::new(step)),
            timeout: None,
        });
    }

    /// Registers a step that needs a runtime.
    pub fn register_async(
        &mut self,
        name: impl Into<String>,
        step: impl Future<Output = Result<(), String>> + Send + 'static,
    ) {
        self.steps.push(Step {
            name: name.into(),
            action: Action::Async(Box::pin(step)),
            timeout: None,
        });
    }

    /// Registers a step that needs a runtime and more (or less) time than the teardown's
    /// timeout gives it. Other steps keep the teardown's.
    pub fn register_async_with_timeout(
        &mut self,
        name: impl Into<String>,
        timeout: Duration,
        step: impl Future<Output = Result<(), String>> + Send + 'static,
    ) {
        self.steps.push(Step {
            name: name.into(),
            action: Action::Async(Box::pin(step)),
            timeout: Some(Box::new(move || timeout)),
        });
    }

    /// Like [`Teardown::register_async_with_timeout`], with the timeout read when the step starts
    /// to run, for a step whose time depends on state that can change after it is registered.
    pub fn register_async_with_timeout_fn(
        &mut self,
        name: impl Into<String>,
        timeout: impl Fn() -> Duration + Send + 'static,
        step: impl Future<Output = Result<(), String>> + Send + 'static,
    ) {
        self.steps.push(Step {
            name: name.into(),
            action: Action::Async(Box::pin(step)),
            timeout: Some(Box::new(timeout)),
        });
    }

    /// Whether no step is left to run.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Runs the registered steps, last registered first, each taken as it runs. Returns the
    /// steps that failed, panicked or timed out, in the order they ran. A second call finds
    /// nothing to run.
    pub fn run(&mut self) -> Vec<StepFailure> {
        let mut failures = Vec::new();
        while let Some(step) = self.steps.pop() {
            failures.extend(run_one(step, self.timeout));
        }
        failures
    }
}

/// Runs one step, however it ends, and returns its failure. Nothing outside the step's own work
/// may stop the steps after it: a panic anywhere in handling one step fails that step only.
fn run_one(step: Step, default: Duration) -> Option<StepFailure> {
    let name = step.name.clone();
    catch_unwind(AssertUnwindSafe(|| run_step(step, default)))
        .unwrap_or_else(|payload| Err(format!("panicked: {}", panic_text(payload.as_ref()))))
        .err()
        .map(|reason| StepFailure { name, reason })
}

/// Runs the steps of a teardown shared between handles, last registered first. The lock is held
/// only to take the next step, never while one runs, so a step may register further steps (they
/// run next) and other handles may register meanwhile.
///
/// With `close`, the run is ending: the call that finds no step left closes registration, under
/// the same lock, so a registration either lands before it (and its step runs here) or is
/// refused ([`TeardownClosed`]); none is queued for a drop after the report was written.
pub(crate) fn run_shared(shared: &Mutex<Teardown>, close: bool) -> Vec<StepFailure> {
    let mut failures = Vec::new();
    loop {
        let (step, default) = {
            let mut teardown: MutexGuard<'_, Teardown> =
                shared.lock().unwrap_or_else(PoisonError::into_inner);
            match teardown.steps.pop() {
                Some(step) => (step, teardown.timeout),
                None => {
                    teardown.closed |= close;
                    return failures;
                }
            }
        };
        failures.extend(run_one(step, default));
    }
}

/// Runs one step, with `default` as the timeout of an async step that has none of its own.
fn run_step(step: Step, default: Duration) -> Result<(), String> {
    match step.action {
        Action::Sync(f) => match catch_unwind(AssertUnwindSafe(f)) {
            Ok(result) => result,
            Err(payload) => Err(format!("panicked: {}", panic_text(payload.as_ref()))),
        },
        Action::Async(future) => {
            // The step's timeout is read inside the same panic boundary as the step.
            let timeout = match &step.timeout {
                None => Ok(default),
                Some(timeout) => catch_unwind(AssertUnwindSafe(timeout)).map_err(|payload| {
                    format!("timeout panicked: {}", panic_text(payload.as_ref()))
                }),
            };
            match timeout {
                Ok(timeout) => run_async(future, timeout),
                Err(reason) => {
                    // The step is not run; dropping it must not panic out either.
                    let _ = catch_unwind(AssertUnwindSafe(|| drop(future)));
                    Err(reason)
                }
            }
        }
    }
}

/// What a check gets of its run's teardown (`RunGuard::teardown`): it can register steps and set
/// the timeout, but not run them. Running goes through the run guard, which keeps what failed, so
/// a failure cannot be returned to the check and lost. The handle is cheap to clone and every
/// clone registers into the same run, so several guarded handles (a T0 loopback of two VCI
/// modules) share one teardown, and the steps of all run in one reverse registration order.
#[derive(Clone)]
pub struct TeardownHandle(pub(crate) Arc<Mutex<Teardown>>);

/// The run has ended its teardown, so a step registered now would never run in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the run's teardown has ended; a step registered now would not run")]
pub struct TeardownClosed;

impl TeardownHandle {
    /// The teardown, unless the run closed registration.
    fn open(&self) -> Result<MutexGuard<'_, Teardown>, TeardownClosed> {
        let teardown = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if teardown.closed {
            Err(TeardownClosed)
        } else {
            Ok(teardown)
        }
    }

    /// See [`Teardown::set_timeout`].
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set_timeout(timeout);
    }

    /// See [`Teardown::register`]. Refused once the run has ended its teardown.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        step: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) -> Result<(), TeardownClosed> {
        self.open()?.register(name, step);
        Ok(())
    }

    /// See [`Teardown::register_async`]. Refused once the run has ended its teardown.
    pub fn register_async(
        &mut self,
        name: impl Into<String>,
        step: impl Future<Output = Result<(), String>> + Send + 'static,
    ) -> Result<(), TeardownClosed> {
        self.open()?.register_async(name, step);
        Ok(())
    }

    /// See [`Teardown::register_async_with_timeout`]. Refused once the run has ended its
    /// teardown.
    pub fn register_async_with_timeout(
        &mut self,
        name: impl Into<String>,
        timeout: Duration,
        step: impl Future<Output = Result<(), String>> + Send + 'static,
    ) -> Result<(), TeardownClosed> {
        self.open()?
            .register_async_with_timeout(name, timeout, step);
        Ok(())
    }

    /// See [`Teardown::register_async_with_timeout_fn`]. Refused once the run has ended its
    /// teardown.
    pub fn register_async_with_timeout_fn(
        &mut self,
        name: impl Into<String>,
        timeout: impl Fn() -> Duration + Send + 'static,
        step: impl Future<Output = Result<(), String>> + Send + 'static,
    ) -> Result<(), TeardownClosed> {
        self.open()?
            .register_async_with_timeout_fn(name, timeout, step);
        Ok(())
    }

    /// Whether no step is left to run.
    pub fn is_empty(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }
}

impl Drop for Teardown {
    /// Runs what is left. Never panics: a failure goes to stderr.
    fn drop(&mut self) {
        // `run` is panic-safe per step; should anything still escape it, the steps left run on.
        while !self.steps.is_empty() {
            if let Ok(failures) = catch_unwind(AssertUnwindSafe(|| self.run())) {
                for failure in failures {
                    eprintln!("teardown step failed: {failure}");
                }
            }
        }
    }
}

fn panic_text(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".to_owned())
}

/// Drives `future` to its end, or for at most `timeout`, on a detached thread with a fresh
/// current-thread runtime. The caller waits on a channel, so a step that blocks inside `poll`,
/// where no runtime timer can fire, still times out: the thread is left behind, never joined, and
/// the remaining steps run.
fn run_async(future: AsyncStep, timeout: Duration) -> Result<(), String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("teardown-step".to_owned())
        .spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("cannot start a runtime: {e}"))?;
                // The timer is created inside the runtime, not before it. It ends a step that
                // is merely waiting, so its thread does not outlive the teardown.
                let result =
                    runtime.block_on(async move { tokio::time::timeout(timeout, future).await });
                runtime.shutdown_background();
                result.unwrap_or_else(|_| Err(format!("timed out after {timeout:?}")))
            }))
            .unwrap_or_else(|payload| Err(format!("panicked: {}", panic_text(payload.as_ref()))));
            // The receiver is gone when the caller gave up.
            let _ = sender.send(outcome);
        });
    if let Err(e) = spawned {
        return Err(format!("cannot start a thread: {e}"));
    }
    // A grace period beyond the step's own timer, for the step that cannot be interrupted.
    match receiver.recv_timeout(timeout.saturating_add(Duration::from_millis(250))) {
        Ok(result) => result,
        Err(_) => Err(format!("timed out after {timeout:?}")),
    }
}

#[cfg(test)]
#[path = "teardown_tests.rs"]
mod tests;

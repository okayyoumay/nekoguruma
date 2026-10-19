//! The teardown of a hardware check (design 13.5 "Cleanup", ADR-333 item 5).
//!
//! The harness registers a step as it acquires each resource: before it switches power on, when
//! it opens a device, when it connects a link. [`Teardown::run`] runs the registered steps in
//! reverse order and takes each one as it goes, so a step runs at most once however often `run`
//! is called, and [`Drop`] runs what is left, so the steps run when a check panics too. A step
//! that fails or panics does not stop the others; [`Teardown::run`] returns what failed, and the
//! run's [`crate::report::RunGuard`] turns a failure into a failed check.
//!
//! An async step is driven on a scoped OS thread with its own current-thread runtime and a
//! timeout, so `run` can be called from anywhere: blocking on a runtime from inside a runtime's
//! worker thread panics, and a thread of its own does not.

use std::any::Any;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::time::Duration;

/// How long an async step may take before it is given up on, unless set otherwise.
pub const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(30);

type SyncStep = Box<dyn FnOnce() -> Result<(), String> + Send>;
type AsyncStep = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

enum Action {
    Sync(SyncStep),
    Async(AsyncStep),
}

struct Step {
    name: String,
    action: Action,
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
            let result = match step.action {
                Action::Sync(f) => match catch_unwind(AssertUnwindSafe(f)) {
                    Ok(result) => result,
                    Err(payload) => Err(format!("panicked: {}", panic_text(payload.as_ref()))),
                },
                Action::Async(future) => run_async(future, self.timeout),
            };
            if let Err(reason) = result {
                failures.push(StepFailure {
                    name: step.name,
                    reason,
                });
            }
        }
        failures
    }
}

impl Drop for Teardown {
    /// Runs what is left. Never panics: a failure goes to stderr.
    fn drop(&mut self) {
        let failures = catch_unwind(AssertUnwindSafe(|| self.run())).unwrap_or_default();
        for failure in failures {
            eprintln!("teardown step failed: {failure}");
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

/// Drives `future` to its end, or for at most `timeout`, on a thread of its own with a fresh
/// current-thread runtime.
fn run_async(future: AsyncStep, timeout: Duration) -> Result<(), String> {
    std::thread::scope(|scope| {
        let spawned = std::thread::Builder::new()
            .name("teardown-step".to_owned())
            .spawn_scoped(scope, move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("cannot start a runtime: {e}"))?;
                // The timer is created inside the runtime, not before it.
                let result =
                    runtime.block_on(async move { tokio::time::timeout(timeout, future).await });
                // A step that left tasks behind does not hold the teardown up.
                runtime.shutdown_background();
                match result {
                    Ok(result) => result,
                    Err(_) => Err(format!("timed out after {timeout:?}")),
                }
            });
        match spawned {
            Ok(handle) => match handle.join() {
                Ok(result) => result,
                Err(payload) => Err(format!("panicked: {}", panic_text(payload.as_ref()))),
            },
            Err(e) => Err(format!("cannot start a thread: {e}")),
        }
    })
}

#[cfg(test)]
#[path = "teardown_tests.rs"]
mod tests;

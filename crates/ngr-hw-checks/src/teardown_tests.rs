use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::profile::parse;
use crate::report::{Outcome, RunGuard};

type Log = Arc<Mutex<Vec<&'static str>>>;

fn log() -> Log {
    Arc::default()
}

fn step(log: &Log, name: &'static str) -> impl FnOnce() -> Result<(), String> + Send + 'static {
    let log = Arc::clone(log);
    move || {
        log.lock().unwrap().push(name);
        Ok(())
    }
}

fn events(log: &Log) -> Vec<&'static str> {
    log.lock().unwrap().clone()
}

#[test]
fn steps_run_in_reverse_and_only_once() {
    let log = log();
    let mut teardown = Teardown::new();
    teardown.register("power", step(&log, "power"));
    teardown.register("device", step(&log, "device"));
    teardown.register("link", step(&log, "link"));
    assert!(teardown.run().is_empty());
    assert_eq!(events(&log), ["link", "device", "power"]);
    // A second run, and the drop after it, find nothing.
    assert!(teardown.is_empty());
    assert!(teardown.run().is_empty());
    drop(teardown);
    assert_eq!(events(&log).len(), 3);
}

#[test]
fn steps_registered_before_a_panic_each_run_once() {
    let log = log();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut teardown = Teardown::new();
        teardown.register("power", step(&log, "power"));
        teardown.register("device", step(&log, "device"));
        teardown.register("link", step(&log, "link"));
        panic!("the check failed");
    }));
    assert!(result.is_err());
    // The drop during the unwind ran them, in reverse.
    assert_eq!(events(&log), ["link", "device", "power"]);
}

#[test]
fn a_run_that_panicked_between_registrations_runs_what_was_registered() {
    let log = log();
    let mut teardown = Teardown::new();
    let result = catch_unwind(AssertUnwindSafe(|| {
        teardown.register("power", step(&log, "power"));
        teardown.register("device", step(&log, "device"));
        panic!("the device did not open");
    }));
    assert!(result.is_err());
    assert!(teardown.run().is_empty());
    assert_eq!(events(&log), ["device", "power"]);
}

#[test]
fn a_failing_or_panicking_step_does_not_stop_the_rest() {
    let log = log();
    let mut teardown = Teardown::new();
    teardown.register("power", step(&log, "power"));
    teardown.register("device", || Err("the device is busy".to_owned()));
    teardown.register("session", || panic!("no answer from the ECU"));
    teardown.register("link", step(&log, "link"));
    let failures = teardown.run();
    assert_eq!(events(&log), ["link", "power"]);
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0].name, "session");
    assert!(failures[0].reason.contains("no answer from the ECU"));
    assert_eq!(failures[1].name, "device");
    assert_eq!(failures[1].reason, "the device is busy");
}

#[test]
fn dropping_never_panics_even_when_a_step_does() {
    let mut teardown = Teardown::new();
    teardown.register("session", || panic!("no answer from the ECU"));
    let result = catch_unwind(AssertUnwindSafe(move || drop(teardown)));
    assert!(result.is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn an_async_step_runs_from_inside_a_runtime() {
    let log = log();
    let mut teardown = Teardown::new();
    teardown.register("power", step(&log, "power"));
    let inner = Arc::clone(&log);
    teardown.register_async("channel", async move {
        // The step has a runtime of its own, with timers.
        tokio::time::sleep(Duration::from_millis(10)).await;
        inner.lock().unwrap().push("channel");
        Ok(())
    });
    assert!(teardown.run().is_empty());
    assert_eq!(events(&log), ["channel", "power"]);
}

#[test]
fn an_async_step_that_hangs_times_out_and_the_rest_runs() {
    let log = log();
    let mut teardown = Teardown::new();
    teardown.set_timeout(Duration::from_millis(100));
    teardown.register("power", step(&log, "power"));
    teardown.register_async("channel", std::future::pending());
    let failures = teardown.run();
    assert_eq!(events(&log), ["power"]);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].reason.contains("timed out"), "{failures:?}");
}

#[test]
fn an_async_step_that_fails_or_panics_is_reported() {
    let mut teardown = Teardown::new();
    teardown.register_async("channel", async { Err("closed twice".to_owned()) });
    teardown.register_async("device", async { panic!("the library crashed") });
    let failures = teardown.run();
    assert_eq!(failures.len(), 2);
    assert!(failures[0].reason.contains("the library crashed"));
    assert_eq!(failures[1].reason, "closed twice");
}

fn t3_profile() -> crate::Profile {
    parse(
        r#"
class = "T3"

[vci]
api = "j2534-0404"
library = "Vendor VCI"
module = "VCI-1"
label = "bench"
pname = "Vendor VCI 1"
firmware_version = "1.2.3"
library_version = "4.5"

[link]
protocol = "ISO_14229_3_on_ISO_15765_2"
baud_rate = 500000
pins = [6, 14]
request_id = 0x7E0
response_id = 0x7E8
id_bits = 11
addressing = "normal"
"#,
    )
    .unwrap()
}

fn only_report(dir: &std::path::Path) -> toml::Table {
    let file = std::fs::read_dir(dir).unwrap().next().unwrap().unwrap();
    std::fs::read_to_string(file.path())
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn a_failed_step_turns_a_recorded_pass_into_a_failed_check() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("reports");
    let log = log();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut run = RunGuard::start_in(&t3_profile(), "hw_t", dir.clone());
        run.teardown().register("power", step(&log, "power"));
        run.teardown()
            .register("device", || Err("the device is busy".to_owned()));
        run.pass();
    }));
    // The guard's drop fails the test, and the other step ran.
    assert!(result.is_err());
    assert_eq!(events(&log), ["power"]);
    let table = only_report(&dir);
    let checks = table["check"].as_array().unwrap();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["outcome"].as_str(), Some("failed"));
    let message = checks[0]["message"].as_str().unwrap();
    assert!(message.contains("teardown failed"), "{message}");
    assert!(message.contains("device"), "{message}");
}

#[test]
fn teardown_runs_when_the_check_panics_and_a_clean_one_stays_a_pass() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("reports");
    let log = log();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut run = RunGuard::start_in(&t3_profile(), "hw_p", dir.clone());
        run.teardown().register("power", step(&log, "power"));
        run.teardown().register("device", step(&log, "device"));
        run.teardown().register("link", step(&log, "link"));
        panic!("an assertion failed");
    }));
    assert!(result.is_err());
    assert_eq!(events(&log), ["link", "device", "power"]);
    let table = only_report(&dir);
    assert_eq!(
        table["check"].as_array().unwrap()[0]["outcome"].as_str(),
        Some("failed")
    );

    // Steps that succeed leave a recorded pass as it is.
    let dir = root.path().join("clean");
    let log = self::log();
    {
        let mut run = RunGuard::start_in(&t3_profile(), "hw_c", dir.clone());
        run.teardown().register("power", step(&log, "power"));
        run.pass();
    }
    assert_eq!(events(&log), ["power"]);
    let table = only_report(&dir);
    assert_eq!(
        table["check"].as_array().unwrap()[0]["outcome"].as_str(),
        Some("passed")
    );
}

#[test]
fn a_failed_step_is_added_to_a_recorded_failure() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("reports");
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut run = RunGuard::start_in(&t3_profile(), "hw_f", dir.clone());
        run.teardown()
            .register("power", || Err("the relay did not answer".to_owned()));
        run.finish(Outcome::Failed, "no response to the read");
    }));
    assert!(result.is_err());
    let table = only_report(&dir);
    let message = table["check"].as_array().unwrap()[0]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("no response to the read"), "{message}");
    assert!(message.contains("the relay did not answer"), "{message}");
}

#[test]
fn an_async_step_that_blocks_inside_poll_times_out_and_the_rest_runs() {
    let log = log();
    let mut teardown = Teardown::new();
    teardown.set_timeout(Duration::from_millis(100));
    teardown.register("power", step(&log, "power"));
    // Blocks the thread that polls it, where no runtime timer can fire.
    teardown.register_async("channel", async {
        std::thread::sleep(Duration::from_secs(2));
        Ok(())
    });
    let started = std::time::Instant::now();
    let failures = teardown.run();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(events(&log), ["power"]);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].reason.contains("timed out"), "{failures:?}");
}

#[test]
fn an_early_teardown_run_keeps_its_failures_for_the_check() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("reports");
    let log = log();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut run = RunGuard::start_in(&t3_profile(), "hw_e", dir.clone());
        run.teardown()
            .register("device", || Err("the device is busy".to_owned()));
        run.teardown().register("power", step(&log, "power"));
        // Early cleanup: the failure is returned for information and kept by the guard.
        let failures = run.run_teardown();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "device");
        assert_eq!(events(&log), ["power"]);
        // A step registered after the early run runs at drop.
        run.teardown().register("link", step(&log, "link"));
        run.pass();
    }));
    // The check recorded a pass, but the guard fails it, naming the step.
    assert!(result.is_err());
    assert_eq!(events(&log), ["power", "link"]);
    let table = only_report(&dir);
    let checks = table["check"].as_array().unwrap();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["outcome"].as_str(), Some("failed"));
    let message = checks[0]["message"].as_str().unwrap();
    assert!(
        message.contains("teardown failed") && message.contains("device"),
        "{message}"
    );
}

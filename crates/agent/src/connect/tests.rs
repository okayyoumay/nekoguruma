//! The key's retry and the error mapping of `ngr-agent connect`, on a paused clock. The whole
//! command is driven as a process by `tests/connect.rs`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::identity::load_or_create;
use crate::identity::store::{MemoryStore, SecretName};

fn unavailable() -> IdentityError {
    IdentityError::Unavailable("the key store is unavailable: test".to_owned())
}

fn some_key() -> AgentKey {
    let store = MemoryStore::new();
    let name = SecretName::agent_key(OperatingMode::User);
    let lock_dir = tempfile::tempdir().unwrap();
    load_or_create(
        &store,
        &name,
        &|| Ok(lock_dir.path().to_owned()),
        Duration::from_millis(100),
    )
    .unwrap()
}

/// Calls `load` until it settles, with `stop` resolving after `stop_after`.
async fn retrying(
    load: impl Fn() -> Result<AgentKey, IdentityError> + Clone + Send + 'static,
    stop_after: Duration,
) -> Result<Option<AgentKey>, ConnectError> {
    let stop = tokio::time::sleep(stop_after);
    tokio::pin!(stop);
    load_key_retrying(load, stop.as_mut()).await
}

#[tokio::test(start_paused = true)]
async fn an_unavailable_store_is_tried_again_with_doubling_delays_until_the_stop() {
    let started = Instant::now();
    let times = Arc::new(Mutex::new(Vec::new()));
    let seen = times.clone();
    let result = retrying(
        move || {
            seen.lock().unwrap().push(started.elapsed());
            Err(unavailable())
        },
        Duration::from_secs(100),
    )
    .await;
    assert!(matches!(result, Ok(None)));
    let secs: Vec<u64> = times.lock().unwrap().iter().map(|t| t.as_secs()).collect();
    // 0, +1, +2, +4, +8, +16, +32, then the cap: +60 would be 123, after the stop.
    assert_eq!(secs, [0, 1, 3, 7, 15, 31, 63]);
}

#[tokio::test(start_paused = true)]
async fn the_delay_stops_doubling_at_the_cap() {
    let started = Instant::now();
    let times = Arc::new(Mutex::new(Vec::new()));
    let seen = times.clone();
    let result = retrying(
        move || {
            seen.lock().unwrap().push(started.elapsed());
            Err(unavailable())
        },
        Duration::from_secs(300),
    )
    .await;
    assert!(matches!(result, Ok(None)));
    let secs: Vec<u64> = times.lock().unwrap().iter().map(|t| t.as_secs()).collect();
    assert_eq!(secs, [0, 1, 3, 7, 15, 31, 63, 123, 183, 243]);
}

#[tokio::test(start_paused = true)]
async fn a_store_that_comes_back_gives_its_key() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let key = some_key();
    let fingerprint = key.public_key().fingerprint_hex();
    let slot = Arc::new(Mutex::new(Some(key)));
    let result = retrying(
        move || {
            if counted.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(unavailable())
            } else {
                Ok(slot.lock().unwrap().take().unwrap())
            }
        },
        Duration::from_secs(100),
    )
    .await;
    let key = result.unwrap().expect("a key, not a stop");
    assert_eq!(key.public_key().fingerprint_hex(), fingerprint);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn the_other_store_errors_end_the_load_without_a_retry() {
    for (error, exit) in [
        (IdentityError::KeyUnusable("test"), 4),
        (IdentityError::NoStore("test"), 1),
        (IdentityError::NotProvisioned, 1),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let error = Arc::new(Mutex::new(Some(error)));
        let result = retrying(
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Err(error.lock().unwrap().take().unwrap())
            },
            Duration::from_secs(100),
        )
        .await;
        let error = result.unwrap_err();
        assert_eq!(error.exit_code(), exit, "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn every_control_error_has_the_documented_exit() {
    let record = crate::registration::record::RecordError::Json;
    for (error, exit) in [
        (ControlError::AlreadyRunning, 3),
        (ControlError::RegistrationRunning, 3),
        (ControlError::Lock("x".to_owned()), 3),
        (ControlError::NotRegistered, 1),
        (ControlError::Record(record), 1),
        (ControlError::KeyMismatch, 1),
    ] {
        let mapped = control_error(&error);
        assert_eq!(mapped.exit_code(), exit, "{mapped}");
    }
    assert!(
        control_error(&ControlError::NotRegistered)
            .to_string()
            .contains("not registered")
    );
    assert!(
        control_error(&ControlError::KeyMismatch)
            .to_string()
            .contains("register again")
    );
}

//! The key's retry, the end of the session and the error mapping of `ngr-agent connect`, on a
//! paused clock. The whole
//! command is driven as a process by `tests/connect.rs`.

use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use shared_crypto::{Signer, SigningKey};
use tokio::time::Instant;
use tracing_subscriber::fmt::MakeWriter;

use super::*;
use crate::identity::load_or_create;
use crate::identity::store::{MemoryStore, SecretName};
use crate::registration::record::{self, Record, RecordError};

const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";

/// A captured operational log.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl io::Write for Log {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Log {
    type Writer = Log;

    fn make_writer(&'a self) -> Log {
        self.clone()
    }
}

impl Log {
    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .finish(),
        )
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// The state directories with a record naming `key`, as `open_in` wants them.
struct Dirs {
    dir: tempfile::TempDir,
}

impl Dirs {
    fn new(key: &AgentKey) -> Dirs {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("control")).unwrap();
        std::fs::create_dir(dir.path().join("registration")).unwrap();
        let instruction = SigningKey::generate().unwrap().public_key();
        let record = Record {
            agent_id: AGENT_ID.to_owned(),
            owner_id: "owner-1".to_owned(),
            preset: "personal".to_owned(),
            allow_remote: false,
            server_origin: "http://127.0.0.1:1".to_owned(),
            instruction_key_pem: instruction.to_spki_pem(),
            instruction_key_id: instruction.fingerprint_hex(),
            agent_key_fingerprint: key.public_key().fingerprint_hex(),
        };
        std::fs::write(
            dir.path().join("registration").join(record::RECORD_FILE),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        Dirs { dir }
    }

    fn open(&self) -> Result<control::Opened, ControlError> {
        control::open_in(
            &self.dir.path().join("control"),
            &self.dir.path().join("registration"),
        )
    }
}

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

/// A key as the store gives it on a later run: not created by this call.
fn existing_key() -> AgentKey {
    let store = MemoryStore::new();
    let name = SecretName::agent_key(OperatingMode::User);
    let lock_dir = tempfile::tempdir().unwrap();
    let load = || {
        load_or_create(
            &store,
            &name,
            &|| Ok(lock_dir.path().to_owned()),
            Duration::from_millis(100),
        )
        .unwrap()
    };
    assert!(load().created());
    load()
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
        (
            IdentityError::LocalFault("the creation lock cannot be used".to_owned()),
            3,
        ),
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

/// A fault no retry fixes carries its reason, as `register` prints it.
#[test]
fn a_local_fault_is_exit_3_with_its_reason() {
    let mapped = identity_error(&IdentityError::LocalFault(
        "the state directory cannot be used: refused".to_owned(),
    ));
    assert_eq!(mapped.exit_code(), 3);
    assert!(mapped.to_string().contains("refused"), "{mapped}");
    let mapped = identity_error(&IdentityError::NoStore("the reason"));
    assert!(mapped.to_string().contains("the reason"), "{mapped}");
    let mapped = identity_error(&IdentityError::KeyUnusable("the other reason"));
    assert!(mapped.to_string().contains("the other reason"), "{mapped}");
}

/// A creation lock held past its wait is tried again, and the warning names a fixed class.
#[tokio::test(start_paused = true)]
async fn a_busy_creation_lock_is_tried_again_and_the_class_is_named() {
    let log = Log::default();
    let _guard = log.capture();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let key = some_key();
    let slot = Arc::new(Mutex::new(Some(key)));
    let result = retrying(
        move || {
            if counted.fetch_add(1, Ordering::SeqCst) < 1 {
                Err(IdentityError::CreationLockBusy)
            } else {
                Ok(slot.lock().unwrap().take().unwrap())
            }
        },
        Duration::from_secs(100),
    )
    .await;
    assert!(result.unwrap().is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let text = log.text();
    assert!(text.contains("creation lock"), "{text}");
    assert!(!text.contains("key store"), "{text}");
}

/// A stop during the key retry ends the command with 0 and both locks free again.
#[tokio::test(start_paused = true)]
async fn a_stop_during_the_key_retry_is_ok_and_frees_the_locks() {
    let log = Log::default();
    let _guard = log.capture();
    let dirs = Dirs::new(&some_key());
    let opened = dirs.open().unwrap();
    let result = run_opened(
        opened,
        || Err(unavailable()),
        |_, _| unreachable!("the session must not start"),
        tokio::time::sleep(Duration::from_secs(5)),
    )
    .await;
    assert!(result.is_ok());
    let text = log.text();
    assert!(text.contains("stopped before it started"), "{text}");
    assert!(text.contains("key store"), "{text}");
    dirs.open().expect("the locks are free");
}

/// A session task that ends without a stop (a panic) is exit 1, not a process that stays up.
#[tokio::test(start_paused = true)]
async fn a_session_that_ends_on_its_own_is_exit_1() {
    let key = existing_key();
    let dirs = Dirs::new(&key);
    let opened = dirs.open().unwrap();
    let slot = Arc::new(Mutex::new(Some(key)));
    let result = run_opened(
        opened,
        move || Ok(slot.lock().unwrap().take().unwrap()),
        |opened, key| opened.start_panicking(key),
        std::future::pending::<()>(),
    )
    .await;
    let error = result.unwrap_err();
    assert_eq!(error.exit_code(), 1);
    assert_eq!(
        error.to_string(),
        "the control channel session ended unexpectedly"
    );
    dirs.open().expect("the locks are free");
}

#[test]
fn every_control_error_has_the_documented_exit() {
    let record = RecordError::Json;
    for (error, exit) in [
        (
            ControlError::Record(RecordError::Io(io::ErrorKind::PermissionDenied)),
            3,
        ),
        (ControlError::Record(RecordError::NotARecord), 1),
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
    // A record that cannot be read for an I/O reason does not say to register again.
    let unreadable = control_error(&ControlError::Record(RecordError::Io(
        io::ErrorKind::PermissionDenied,
    )))
    .to_string();
    assert!(unreadable.contains("PermissionDenied"), "{unreadable}");
    assert!(!unreadable.contains("register again"), "{unreadable}");
    assert!(
        control_error(&ControlError::KeyMismatch)
            .to_string()
            .contains("register again")
    );
}

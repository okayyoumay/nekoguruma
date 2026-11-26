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
use crate::log_upload::NoLogUploads;
use crate::outbox::{Outbox, OutboxError};
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

/// The origin the fixture's record names, which the installation file names too.
const RECORD_ORIGIN: &str = "http://127.0.0.1:1";

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
            server_origin: RECORD_ORIGIN.to_owned(),
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
            &ServerOrigin::parse(RECORD_ORIGIN).unwrap(),
        )
    }
}

/// The outbox of the record's agent in `dir`, as `run` opens it.
fn outbox_in(dir: &std::path::Path) -> control::SharedOutbox {
    let dir = dir.join("outbox");
    std::fs::create_dir_all(&dir).unwrap();
    control::SharedOutbox::new(Outbox::open_in(&dir, AGENT_ID).unwrap())
}

/// The issue-time mark of the record's agent in `dir`, as `run` opens it.
fn marks_in(dir: &std::path::Path) -> IssueMark {
    IssueMark::open_in(&dir.join("registration"), AGENT_ID).unwrap()
}

/// The received-jobs ledger of the record's agent in `dir`, as `run` opens it.
fn ledger_in(dir: &std::path::Path) -> ReceivedJobs {
    ReceivedJobs::open_in(&dir.join("registration"), AGENT_ID).unwrap()
}

/// The upload ledger of the record's agent in `dir`, as `run` opens it.
fn uploads_in(dir: &std::path::Path) -> uploader::SharedLedger {
    let dir = dir.join("uploads");
    std::fs::create_dir_all(&dir).unwrap();
    uploader::SharedLedger::new(UploadLedger::open_in(&dir, AGENT_ID).unwrap())
}

/// The real session over `dir`'s state, which dials the record's closed port.
fn start_session(
    opened: control::Opened,
    key: &AgentKey,
    dir: &std::path::Path,
) -> Result<control::Handle, ControlError> {
    opened.start(
        key,
        Collaborators {
            intake: Arc::new(LogIntake),
            outbox: outbox_in(dir),
            marks: marks_in(dir),
            clock: Arc::new(SystemClock),
            ledger: ledger_in(dir),
            log_uploads: Arc::new(NoLogUploads),
        },
    )
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
        |_, _| unreachable!("the uploader must not start"),
        |_, _| unreachable!("the session must not start"),
        UPLOADER_STOP_BOUND,
        tokio::time::sleep(Duration::from_secs(5)),
    )
    .await;
    result.unwrap();
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
        |record, key| uploader::start(record, key, uploads_in(dirs.dir.path())),
        |opened, key| {
            opened.start_panicking(
                key,
                outbox_in(dirs.dir.path()),
                marks_in(dirs.dir.path()),
                ledger_in(dirs.dir.path()),
            )
        },
        UPLOADER_STOP_BOUND,
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

/// An uploader that ends on its own (its ledger does not read back) is logged once at error level
/// and the session goes on: the command ends only by the stop, with 0 (ADR-493 item 10).
#[tokio::test(start_paused = true)]
async fn an_uploader_that_ends_on_its_own_does_not_end_the_session() {
    use crate::upload_ledger::Step;
    let log = Log::default();
    let _guard = log.capture();
    let key = existing_key();
    let dirs = Dirs::new(&key);
    let opened = dirs.open().unwrap();
    let slot = Arc::new(Mutex::new(Some(key)));
    // An append that lands and whose sync fails poisons the ledger; the file is then replaced by
    // one that does not read back, so the uploader's reopen ends it.
    let uploads_dir = dirs.dir.path().join("uploads");
    std::fs::create_dir_all(&uploads_dir).unwrap();
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe_armed = armed.clone();
    let probe: crate::upload_ledger::Probe = Box::new(move |step| {
        if step == Step::AfterSync && probe_armed.load(Ordering::SeqCst) {
            Err(io::Error::other("scripted"))
        } else {
            Ok(())
        }
    });
    let ledger = uploader::SharedLedger::new(
        UploadLedger::open_with(&uploads_dir, AGENT_ID, probe).unwrap(),
    );
    armed.store(true, Ordering::SeqCst);
    let key_of = crate::journal::JobKey {
        job_id: shared_proto::JobId("0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f".to_owned()),
        generation: 1,
    };
    ledger
        .enqueue_result(key_of, vec![1, 2, 3])
        .await
        .unwrap_err();
    std::fs::write(uploads_dir.join(AGENT_ID), b"not a ledger").unwrap();
    let result = run_opened(
        opened,
        move || Ok(slot.lock().unwrap().take().unwrap()),
        move |record, key| uploader::start(record, key, ledger),
        |opened, key| {
            opened.start(
                key,
                Collaborators {
                    intake: Arc::new(LogIntake),
                    outbox: outbox_in(dirs.dir.path()),
                    marks: marks_in(dirs.dir.path()),
                    clock: Arc::new(SystemClock),
                    ledger: ledger_in(dirs.dir.path()),
                    log_uploads: Arc::new(NoLogUploads),
                },
            )
        },
        UPLOADER_STOP_BOUND,
        tokio::time::sleep(Duration::from_secs(5)),
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    let text = log.text();
    assert_eq!(text.matches("the uploader ended").count(), 1, "{text}");
    assert!(!text.contains("uploader stopped"), "{text}");
    assert!(text.contains("control channel session stopped"), "{text}");
    dirs.open().expect("the locks are free");
}

/// A record or a key the session would refuse starts neither the uploader nor the session: a
/// record with an instruction key that does not parse does not open (exit 1), and a key that is not
/// the record's is exit 1 before the uploader starts, with nothing sent.
#[tokio::test(start_paused = true)]
async fn a_record_or_key_the_session_refuses_starts_neither_task() {
    let key = existing_key();
    let dirs = Dirs::new(&some_key());
    let opened = dirs.open().unwrap();
    let slot = Arc::new(Mutex::new(Some(key)));
    let result = run_opened(
        opened,
        move || Ok(slot.lock().unwrap().take().unwrap()),
        |_, _| unreachable!("the uploader must not start"),
        |_, _| unreachable!("the session must not start"),
        UPLOADER_STOP_BOUND,
        std::future::pending::<()>(),
    )
    .await;
    assert_eq!(result.unwrap_err().exit_code(), 1);
    dirs.open().expect("the locks are free");

    let file = dirs
        .dir
        .path()
        .join("registration")
        .join(record::RECORD_FILE);
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    json["instruction_key_pem"] = "not a key".into();
    std::fs::write(&file, serde_json::to_vec(&json).unwrap()).unwrap();
    let Err(error) = dirs.open() else {
        panic!("the record does not open");
    };
    assert_eq!(control_error(&error).exit_code(), 1);
}

/// A stop stops both tasks: the uploader's stop is logged once, no timeout is, and its ledger's
/// lock is free afterwards.
#[tokio::test(start_paused = true)]
async fn a_stop_stops_the_uploader_and_frees_its_ledger() {
    let log = Log::default();
    let _guard = log.capture();
    let key = existing_key();
    let dirs = Dirs::new(&key);
    let opened = dirs.open().unwrap();
    let slot = Arc::new(Mutex::new(Some(key)));
    let ledger = uploads_in(dirs.dir.path());
    let result = run_opened(
        opened,
        move || Ok(slot.lock().unwrap().take().unwrap()),
        move |record, key| uploader::start(record, key, ledger),
        |opened, key| start_session(opened, key, dirs.dir.path()),
        UPLOADER_STOP_BOUND,
        tokio::time::sleep(Duration::from_secs(5)),
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    let text = log.text();
    assert_eq!(text.matches("uploader stopped").count(), 1, "{text}");
    assert!(!text.contains("did not stop in time"), "{text}");
    UploadLedger::open_in(&dirs.dir.path().join("uploads"), AGENT_ID)
        .expect("the uploader released the ledger");
}

/// An uploader that cannot stop in time (a ledger write stalled) is waited for 10 seconds at most:
/// the warning is logged and the command still ends with 0.
#[tokio::test]
async fn an_uploader_that_does_not_stop_is_waited_for_at_most_the_bound() {
    use crate::upload_ledger::Step;
    let log = Log::default();
    let _guard = log.capture();
    let key = existing_key();
    let dirs = Dirs::new(&key);
    let opened = dirs.open().unwrap();
    let slot = Arc::new(Mutex::new(Some(key)));
    let uploads_dir = dirs.dir.path().join("uploads");
    std::fs::create_dir_all(&uploads_dir).unwrap();
    // The first enqueue stalls inside the ledger, holding its lock; the uploader's first pass
    // waits for it.
    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let entered_tx = Mutex::new(entered_tx);
    let probe: crate::upload_ledger::Probe = Box::new(move |step| {
        if step == Step::BeforeWrite {
            let _ = entered_tx.lock().unwrap().send(());
            let _ = release_rx.lock().unwrap().recv();
        }
        Ok(())
    });
    let ledger = uploader::SharedLedger::new(
        UploadLedger::open_with(&uploads_dir, AGENT_ID, probe).unwrap(),
    );
    let stalled = ledger.clone();
    let job = crate::journal::JobKey {
        job_id: shared_proto::JobId("0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f".to_owned()),
        generation: 1,
    };
    let enqueue = tokio::spawn(async move { stalled.enqueue_result(job, vec![1, 2, 3]).await });
    tokio::task::spawn_blocking(move || entered_rx.recv().unwrap())
        .await
        .unwrap();
    let result = run_opened(
        opened,
        move || Ok(slot.lock().unwrap().take().unwrap()),
        move |record, key| uploader::start(record, key, ledger),
        |opened, key| start_session(opened, key, dirs.dir.path()),
        Duration::from_millis(200),
        tokio::time::sleep(Duration::from_millis(100)),
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    let text = log.text();
    assert!(text.contains("did not stop in time"), "{text}");
    assert!(!text.contains("uploader stopped"), "{text}");
    release_tx.send(()).unwrap();
    enqueue.await.unwrap().unwrap();
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
        (ControlError::ServerMismatch, 1),
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
    // H3: a record for another server says to register again, and names no origin.
    let mismatch = control_error(&ControlError::ServerMismatch).to_string();
    assert!(mismatch.contains("register again"), "{mismatch}");
    assert!(mismatch.contains("register --replace"), "{mismatch}");
    assert!(!mismatch.contains("127.0.0.1"), "{mismatch}");
}

/// ADR-478: an outbox that does not read back is exit 1 with the advice to register again; the
/// other failures to open it are exit 3, and none names a path.
#[test]
fn every_outbox_error_has_the_documented_exit() {
    for (error, exit) in [
        (OutboxError::Corrupt("a bad frame"), 1),
        (OutboxError::BadAgentId, 1),
        (OutboxError::InUse, 3),
        (
            OutboxError::Io(io::Error::from(io::ErrorKind::PermissionDenied)),
            3,
        ),
        (OutboxError::Poisoned, 3),
    ] {
        let mapped = outbox_error(&error);
        assert_eq!(mapped.exit_code(), exit, "{mapped}");
    }
    assert!(
        outbox_error(&OutboxError::Corrupt("x"))
            .to_string()
            .contains("register again")
    );
}

/// ADR-486: an issue-time mark that does not read back is exit 1 with the advice to register
/// again; the other failures to open it are exit 3, and none names a path.
#[test]
fn every_mark_error_has_the_documented_exit() {
    for (error, exit) in [
        (MarkError::Corrupt("a bad file"), 1),
        (MarkError::BadAgentId, 1),
        (MarkError::InUse, 3),
        (MarkError::Io(io::ErrorKind::PermissionDenied), 3),
    ] {
        let mapped = mark_error(&error);
        assert_eq!(mapped.exit_code(), exit, "{mapped}");
    }
    assert!(
        mark_error(&MarkError::Corrupt("x"))
            .to_string()
            .contains("register again")
    );
    assert!(
        !mark_error(&MarkError::Io(io::ErrorKind::PermissionDenied))
            .to_string()
            .contains("register again")
    );
}

/// A corrupt mark file does not open, whatever the outbox did, and a second store of the same
/// directory (a session still running) is in use, which is exit 3.
#[test]
fn a_corrupt_mark_file_does_not_open_and_one_in_use_is_exit_3() {
    let key = existing_key();
    let dirs = Dirs::new(&key);
    let registration = dirs.dir.path().join("registration");
    let file = registration.join(format!("{AGENT_ID}.issue-mark.v1.json"));
    std::fs::write(&file, b"not a mark").unwrap();
    let error =
        IssueMark::open_in(&registration, AGENT_ID).expect_err("a corrupt file does not open");
    assert!(matches!(error, MarkError::Corrupt(_)), "{error}");
    assert_eq!(mark_error(&error).exit_code(), 1);
    std::fs::remove_file(&file).unwrap();
    let first = IssueMark::open_in(&registration, AGENT_ID).unwrap();
    let error = IssueMark::open_in(&registration, AGENT_ID).expect_err("a second store is in use");
    assert_eq!(mark_error(&error).exit_code(), 3);
    drop(first);
}

/// A corrupt outbox file ends `connect` with exit 1 before any key is loaded, and both locks are
/// released. The real `run` is not used (it reads the state directory of the mode), so the same
/// two steps are run here.
#[test]
fn a_corrupt_outbox_file_does_not_open_and_maps_to_exit_1() {
    let key = existing_key();
    let dirs = Dirs::new(&key);
    let outbox_dir = dirs.dir.path().join("outbox");
    std::fs::create_dir_all(&outbox_dir).unwrap();
    std::fs::write(
        outbox_dir.join(format!("{AGENT_ID}.outbox")),
        b"not an outbox",
    )
    .unwrap();
    let opened = dirs.open().unwrap();
    let error = Outbox::open_in(&outbox_dir, &opened.record().agent_id)
        .expect_err("a corrupt file does not open");
    assert!(matches!(error, OutboxError::Corrupt(_)), "{error}");
    assert_eq!(outbox_error(&error).exit_code(), 1);
    drop(opened);
    dirs.open().expect("the locks are free");
}

/// ADR-489: a received-jobs ledger that does not read back is exit 1 with the advice to register
/// again; the other failures to open it are exit 3, and none names a path.
#[test]
fn every_ledger_error_has_the_documented_exit() {
    for (error, exit) in [
        (LedgerError::Corrupt("a bad file"), 1),
        (LedgerError::BadAgentId, 1),
        (LedgerError::InUse, 3),
        (LedgerError::Io(io::ErrorKind::PermissionDenied), 3),
        (LedgerError::Poisoned, 3),
    ] {
        let mapped = ledger_error(&error);
        assert_eq!(mapped.exit_code(), exit, "{mapped}");
    }
    assert!(
        ledger_error(&LedgerError::Corrupt("x"))
            .to_string()
            .contains("register again")
    );
    assert!(
        !ledger_error(&LedgerError::Io(io::ErrorKind::PermissionDenied))
            .to_string()
            .contains("register again")
    );
}

#[test]
fn every_upload_ledger_error_has_the_documented_exit() {
    for (error, exit) in [
        (UploadLedgerError::Corrupt("a bad file"), 1),
        (UploadLedgerError::BadAgentId, 1),
        (UploadLedgerError::InUse, 3),
        (
            UploadLedgerError::Io(io::ErrorKind::PermissionDenied.into()),
            3,
        ),
        (UploadLedgerError::Poisoned, 3),
    ] {
        let mapped = upload_ledger_error(&error);
        assert_eq!(mapped.exit_code(), exit, "{mapped}");
    }
    assert!(
        upload_ledger_error(&UploadLedgerError::Corrupt("x"))
            .to_string()
            .contains("register again")
    );
}

/// A corrupt ledger file does not open and maps to exit 1 and is left as it is; a second store of
/// the same directory (a session still running) is in use, which is exit 3.
#[test]
fn a_corrupt_ledger_file_does_not_open_and_one_in_use_is_exit_3() {
    let key = existing_key();
    let dirs = Dirs::new(&key);
    let registration = dirs.dir.path().join("registration");
    let file = registration.join(format!("{AGENT_ID}.received-jobs.v1"));
    std::fs::write(&file, b"not a ledger").unwrap();
    let error =
        ReceivedJobs::open_in(&registration, AGENT_ID).expect_err("a corrupt file does not open");
    assert!(matches!(error, LedgerError::Corrupt(_)), "{error}");
    assert_eq!(ledger_error(&error).exit_code(), 1);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"not a ledger",
        "not repaired"
    );
    std::fs::remove_file(&file).unwrap();
    let first = ReceivedJobs::open_in(&registration, AGENT_ID).unwrap();
    let error =
        ReceivedJobs::open_in(&registration, AGENT_ID).expect_err("a second store is in use");
    assert_eq!(ledger_error(&error).exit_code(), 3);
    drop(first);
}

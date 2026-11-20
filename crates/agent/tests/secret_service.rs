//! The Linux user-mode key store against a real Secret Service (design 6.4, ADR-459 Decision item
//! 4, ADR-460): `agent::identity::secret_service` and `ngr-agent identity` on it.
//!
//! The tests that need a daemon run only when `NGR_TEST_SECRET_SERVICE=1`, and then a missing bus
//! or daemon fails them rather than skipping them. They need a session bus with gnome-keyring's
//! secrets component and an unlocked default collection, as
//!
//! ```text
//! printf pw | gnome-keyring-daemon --unlock --components=secrets
//! ```
//!
//! gives inside `dbus-run-session`. The one test that locks the collection does so in a bus and a
//! daemon of its own, started for it by `dbus-run-session` and `gnome-keyring-daemon` on the
//! `PATH` (a locked collection cannot be unlocked without a prompt, so it never locks the shared
//! one). The tests use names of their own, so they never touch a key of a real agent; the one that runs
//! `ngr-agent identity` cannot, so it runs in a private bus and keyring like the locking test. A nextest test group runs them
//! one at a time (`.config/nextest.toml`).
//!
//! One test needs no daemon: a bus address that leads nowhere gives `Unavailable`.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent::identity::secret_service::SecretService;
use agent::identity::store::{SecretName, SecretStore, StoreErrorKind};
use agent::identity::{IdentityError, LOCK_WAIT, load_or_create};
use zbus::connection::Builder;
use zbus::{Connection, OwnedObjectPath, Variant};

const GATE: &str = "NGR_TEST_SECRET_SERVICE";
const DEST: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const ITEM: &str = "org.freedesktop.Secret.Item";
const COLLECTION: &str = "org.freedesktop.Secret.Collection";
const NAME_ATTRIBUTE: &str = "nekoguruma-secret-name";

/// Every test holds this while it runs, so the one that points `DBUS_SESSION_BUS_ADDRESS`
/// elsewhere never overlaps another under `cargo test`'s threads.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether the real-daemon tests are asked for.
fn enabled() -> bool {
    std::env::var_os(GATE).is_some_and(|value| value == "1")
}

/// A name no other test or process uses.
fn unique_name(tag: &str) -> SecretName {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    SecretName::new(&format!("nekoguruma-test.{tag}.{}.{n}", std::process::id())).unwrap()
}

fn connect() -> Connection {
    zbus::block_on(
        Builder::session()
            .method_timeout(Duration::from_secs(5))
            .build(),
    )
    .expect("a session bus is needed when NGR_TEST_SECRET_SERVICE=1")
}

fn call<B, R>(connection: &Connection, path: &str, interface: &str, method: &str, body: &B) -> R
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
{
    zbus::block_on(async {
        connection
            .call_method(Some(DEST), path, Some(interface), method, body)
            .await?
            .body()
            .deserialize::<R>()
    })
    .unwrap_or_else(|error| panic!("{interface}.{method}: {error}"))
}

fn attrs(name: &SecretName) -> HashMap<&'static str, &str> {
    HashMap::from([(NAME_ATTRIBUTE, name.as_str())])
}

fn default_collection(connection: &Connection) -> String {
    let (path,): (OwnedObjectPath,) = call(
        connection,
        SERVICE_PATH,
        SERVICE,
        "ReadAlias",
        &("default",),
    );
    assert_ne!(path.as_str(), "/", "the daemon has no default collection");
    path.as_str().to_owned()
}

/// The paths of every item holding `name`, locked or not.
fn items(connection: &Connection, name: &SecretName) -> Vec<String> {
    let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = call(
        connection,
        SERVICE_PATH,
        SERVICE,
        "SearchItems",
        &(attrs(name),),
    );
    unlocked
        .iter()
        .chain(&locked)
        .map(|path| path.as_str().to_owned())
        .collect()
}

/// Creates an item through the raw API, as a second writer outside the store would.
fn create_raw(connection: &Connection, name: &SecretName, secret: &[u8]) {
    let collection = default_collection(connection);
    let (_output, session): (zbus::OwnedVariant, OwnedObjectPath) = call(
        connection,
        SERVICE_PATH,
        SERVICE,
        "OpenSession",
        &("plain", Variant::from("")),
    );
    let properties: HashMap<&str, Variant> = HashMap::from([
        (
            "org.freedesktop.Secret.Item.Label",
            Variant::from("Nekoguruma test item"),
        ),
        (
            "org.freedesktop.Secret.Item.Attributes",
            Variant::from(attrs(name)),
        ),
    ]);
    let value = (
        &session,
        Vec::<u8>::new(),
        secret,
        "application/octet-stream",
    );
    let (_item, _prompt): (OwnedObjectPath, OwnedObjectPath) = call(
        connection,
        &collection,
        COLLECTION,
        "CreateItem",
        &(properties, value, false),
    );
}

/// Removes the items of `name` when dropped (also after a failed assertion).
struct Cleanup(SecretName);

impl Drop for Cleanup {
    fn drop(&mut self) {
        // A test that already failed may have no bus to clean up with; a second panic would abort.
        if std::thread::panicking() {
            return;
        }
        let connection = connect();
        for item in items(&connection, &self.0) {
            let (_prompt,): (OwnedObjectPath,) = call(&connection, &item, ITEM, "Delete", &());
        }
    }
}

/// Locks the default collection (for good: the daemon it runs against is the test's own).
fn lock_default() {
    let connection = connect();
    let path = OwnedObjectPath::try_from(default_collection(&connection)).unwrap();
    let (_locked, _prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
        call(&connection, SERVICE_PATH, SERVICE, "Lock", &(vec![path],));
}

fn lock_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn kind<T: std::fmt::Debug>(
    result: Result<T, agent::identity::store::StoreError>,
) -> StoreErrorKind {
    result.unwrap_err().kind()
}

/// Rows 9, 10, 23: absent, then created, then found by a store made afterwards (a restart),
/// and a second create finds the name present.
#[test]
fn a_key_is_created_once_and_found_by_a_fresh_store() {
    let _serial = serial();
    if !enabled() {
        return;
    }
    let name = unique_name("create");
    let _cleanup = Cleanup(name.clone());
    assert_eq!(
        kind(SecretService::new().get(&name)),
        StoreErrorKind::Absent
    );
    SecretService::new().put_new(&name, b"first").unwrap();
    assert_eq!(&**SecretService::new().get(&name).unwrap(), b"first");
    assert_eq!(
        kind(SecretService::new().put_new(&name, b"second")),
        StoreErrorKind::AlreadyExists
    );
    assert_eq!(&**SecretService::new().get(&name).unwrap(), b"first");
    assert_eq!(items(&connect(), &name).len(), 1);
}

/// Row 20: bytes that are no key are returned as stored (`load_or_create` rejects them).
#[test]
fn bytes_come_back_as_stored_and_the_identity_code_rejects_a_bad_key() {
    let _serial = serial();
    if !enabled() {
        return;
    }
    let name = unique_name("badkey");
    let _cleanup = Cleanup(name.clone());
    SecretService::new().put_new(&name, b"not pkcs8").unwrap();
    assert_eq!(&**SecretService::new().get(&name).unwrap(), b"not pkcs8");
    let dir = lock_dir();
    let error = load_or_create(
        &SecretService::new(),
        &name,
        &|| Ok(dir.path().to_owned()),
        LOCK_WAIT,
    )
    .unwrap_err();
    assert!(matches!(error, IdentityError::KeyUnusable(_)), "{error}");
}

/// Rows 9, 23, 24 through `load_or_create`: created once, then the same key.
#[test]
fn load_or_create_creates_once_and_then_finds_the_same_key() {
    let _serial = serial();
    if !enabled() {
        return;
    }
    let name = unique_name("loadcreate");
    let _cleanup = Cleanup(name.clone());
    let dir = lock_dir();
    let lock = || Ok(dir.path().to_owned());
    let first = load_or_create(&SecretService::new(), &name, &lock, LOCK_WAIT).unwrap();
    assert!(first.created());
    let second = load_or_create(&SecretService::new(), &name, &lock, LOCK_WAIT).unwrap();
    assert!(!second.created());
    assert_eq!(second.public_key(), first.public_key());
    assert_eq!(items(&connect(), &name).len(), 1);
}

/// Rows 8, 12, 26: two items under one name are undecryptable, and nothing is created beside
/// them; once they are deleted outside, the name is absent again and a create works.
#[test]
fn two_items_are_undecryptable_and_a_deleted_item_is_absent() {
    let _serial = serial();
    if !enabled() {
        return;
    }
    let name = unique_name("two");
    let cleanup = Cleanup(name.clone());
    let connection = connect();
    create_raw(&connection, &name, b"one");
    create_raw(&connection, &name, b"two");
    assert_eq!(items(&connection, &name).len(), 2);
    let store = SecretService::new();
    assert_eq!(kind(store.get(&name)), StoreErrorKind::Undecryptable);
    assert_eq!(
        kind(store.put_new(&name, b"x")),
        StoreErrorKind::AlreadyExists
    );
    let dir = lock_dir();
    let error =
        load_or_create(&store, &name, &|| Ok(dir.path().to_owned()), LOCK_WAIT).unwrap_err();
    assert!(matches!(error, IdentityError::KeyUnusable(_)), "{error}");
    assert_eq!(items(&connection, &name).len(), 2);
    drop(cleanup);
    assert_eq!(kind(store.get(&name)), StoreErrorKind::Absent);
    let _cleanup = Cleanup(name.clone());
    store.put_new(&name, b"again").unwrap();
    assert_eq!(&**store.get(&name).unwrap(), b"again");
}

/// Rows 7, 8, 11: runs `locked_collection_checks` in a private session bus with a private
/// gnome-keyring, whose collection it locks.
#[test]
fn a_locked_default_collection_is_unavailable_and_nothing_is_created() {
    let _serial = serial();
    if !enabled() {
        return;
    }
    in_private_session("locked_collection_checks");
}

/// Runs the test `inner_test` of this binary in a session bus and a gnome-keyring of its own
/// (private HOME and data directories, the outer bus and keyring variables dropped), so it can
/// only reach a throwaway keyring, and checks that it ran and passed.
fn in_private_session(inner_test: &str) {
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new("dbus-run-session")
        .args(["--", "bash", "-c"])
        .arg(
            "printf pw | gnome-keyring-daemon --unlock --components=secrets >/dev/null \
             && exec \"$@\"",
        )
        .arg("bash")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", inner_test, "--nocapture"])
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", home.path())
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("GNOME_KEYRING_CONTROL")
        .env(INNER, "1")
        .output()
        .expect("dbus-run-session and gnome-keyring-daemon are needed on the PATH");
    let text = String::from_utf8_lossy(&output.stdout);
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{text}\n{errors}");
    assert!(
        text.contains("1 passed"),
        "the inner test did not run: {text}"
    );
}

const INNER: &str = "NGR_TEST_SECRET_SERVICE_INNER";

/// The inside of the test above, in the private session; does nothing elsewhere.
#[test]
fn locked_collection_checks() {
    let _serial = serial();
    if !enabled() || std::env::var_os(INNER).is_none() {
        return;
    }
    let present = unique_name("lockedpresent");
    let missing = unique_name("lockedmissing");
    let store = SecretService::new();
    store.put_new(&present, b"key").unwrap();
    let dir = lock_dir();
    let lock = || Ok(dir.path().to_owned());
    lock_default();
    // Row 8: the item sits in the locked list.
    assert_eq!(kind(store.get(&present)), StoreErrorKind::Unavailable);
    assert_eq!(
        kind(store.put_new(&present, b"other")),
        StoreErrorKind::AlreadyExists
    );
    let error = load_or_create(&store, &present, &lock, LOCK_WAIT).unwrap_err();
    assert!(matches!(error, IdentityError::Unavailable(_)), "{error}");
    // Row 7 as gnome-keyring answers it: a locked collection with nothing found.
    assert_eq!(kind(store.get(&missing)), StoreErrorKind::Unavailable);
    assert_eq!(
        kind(store.put_new(&missing, b"x")),
        StoreErrorKind::Unavailable
    );
    let error = load_or_create(&store, &missing, &lock, LOCK_WAIT).unwrap_err();
    assert!(matches!(error, IdentityError::Unavailable(_)), "{error}");
    // Nothing was created beside the locked item, or for the missing name.
    let connection = connect();
    assert_eq!(items(&connection, &present).len(), 1);
    assert!(items(&connection, &missing).is_empty());
}

/// A bus address that leads nowhere: both calls are `Unavailable`, and so is `ngr-agent identity`
/// (exit 3, nothing written). Needs no daemon.
#[test]
fn no_session_bus_is_unavailable() {
    let _serial = serial();
    const VAR: &str = "DBUS_SESSION_BUS_ADDRESS";
    let saved = std::env::var_os(VAR);
    // SAFETY: every test of this file holds `SERIAL` while it runs, and nothing else in this
    // process reads the environment concurrently.
    unsafe { std::env::set_var(VAR, "unix:path=/nonexistent/ngr-test-bus") };
    let name = unique_name("nobus");
    let store = SecretService::new();
    let started = std::time::Instant::now();
    assert_eq!(kind(store.get(&name)), StoreErrorKind::Unavailable);
    assert_eq!(
        kind(store.put_new(&name, b"x")),
        StoreErrorKind::Unavailable
    );
    let elapsed = started.elapsed();
    // SAFETY: as above.
    unsafe {
        match saved {
            Some(value) => std::env::set_var(VAR, value),
            None => std::env::remove_var(VAR),
        }
    }
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

#[cfg(debug_assertions)]
#[path = "support/installation_file.rs"]
mod installation_file;

#[cfg(debug_assertions)]
mod binary {
    use std::path::Path;
    use std::process::{Command, Output};

    use super::installation_file;
    use super::*;

    /// `ngr-agent identity --mode user` on the Secret Service store, with the account's state
    /// redirected into `home`.
    fn identity(home: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ngr-agent"));
        command
            .args(["identity", "--mode", "user"])
            .env("HOME", home)
            .env("XDG_STATE_HOME", home.join("xdg"))
            .env_remove("NGR_AGENT_STATE_DIR")
            .env("NGR_AGENT_KEY_STORE", "secret-service");
        installation_file::install(&mut command, home, "user", "http://127.0.0.1:1");
        command
    }

    /// Every path below `dir`, as text.
    fn files_under(dir: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_owned()];
        while let Some(next) = pending.pop() {
            for entry in std::fs::read_dir(&next).unwrap() {
                let path = entry.unwrap().path();
                found.push(path.to_string_lossy().into_owned());
                if path.is_dir() {
                    pending.push(path);
                }
            }
        }
        found
    }

    fn json(output: &Output) -> serde_json::Value {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{stderr}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// The restart criterion: two processes, one key. The agent's own name is used, so it runs
    /// in a private session (`restart_checks`) and never touches the developer's keyring.
    #[test]
    fn the_second_process_finds_the_key_the_first_created() {
        let _serial = serial();
        if !enabled() {
            return;
        }
        in_private_session("binary::restart_checks");
    }

    /// The inside of the test above, in the private session; does nothing elsewhere.
    #[test]
    fn restart_checks() {
        let _serial = serial();
        if !enabled() || std::env::var_os(INNER).is_none() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let first = json(&identity(home.path()).output().unwrap());
        let second = json(&identity(home.path()).output().unwrap());
        assert_eq!(first["created"], true);
        assert_eq!(second["created"], false);
        assert_eq!(first["fingerprint"], second["fingerprint"]);
        assert_eq!(first["public_key_pem"], second["public_key_pem"]);
    }

    /// No bus: exit 3, nothing on stdout, and no key or lock file written. Needs no daemon.
    #[test]
    fn without_a_bus_identity_exits_3_and_writes_nothing() {
        let _serial = serial();
        let home = tempfile::tempdir().unwrap();
        let mut command = identity(home.path());
        command.env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/ngr-test-bus",
        );
        let output = command.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        let written = files_under(home.path());
        assert!(
            !written.iter().any(|path| path.contains("identity")),
            "{written:?}"
        );
    }
}

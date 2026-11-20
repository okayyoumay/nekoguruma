//! The bundled object server as a child of the server (ADR-491), against a fake child
//! (`src/bin/ngr-fake-object-server.rs`) that misbehaves on demand: its start and stop, its
//! credentials, its readiness, its end, and what the child inherits. The real pinned binary is
//! tested in `object_server_rustfs.rs`; the units (config, credentials, log forwarding, binary
//! checks) are tested in `src/`.
//!
//! The fake is steered by `fake-object-server.cfg` in its working directory, which is the data
//! directory; each test writes that file before it starts the server.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use server::config::{Config, Database, Http, InstructionKeyConfig, ObjectServerConfig, Profile};
use server::object_server::credentials::{ACCESS_KEY_FILE, SECRET_KEY_FILE};
use server::object_server::{ObjectServer, ObjectServerError, Tuning};
use server::storage::{S3Tuning, StoreError};
use server::{RunError, Server, StartError};

use crate::redeem::{Captured, capture};
use crate::support::{
    TestPostgres, free_port, object_port_lost, retry_on_lost_port, start_object_server,
};

const LIMIT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------------------------
// Helpers

/// Makes `path` owner-only (Unix), as the server's checks want its directories.
fn private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// A data directory with the fake's steering file, and a credentials directory, in a temporary
/// directory; the configuration that names them and the fake.
struct Fixture {
    dir: tempfile::TempDir,
    config: ObjectServerConfig,
    /// The log a test reads, cleared before each attempt of a start, so that it holds the lines
    /// of the final child only.
    log: Option<Captured>,
}

impl Fixture {
    fn new(cfg: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("object-server");
        std::fs::create_dir(&data).unwrap();
        private(&data);
        std::fs::write(data.join("fake-object-server.cfg"), cfg).unwrap();
        let credentials = dir.path().join("credentials");
        std::fs::create_dir(&credentials).unwrap();
        private(&credentials);
        Fixture {
            config: ObjectServerConfig {
                binary: crate::support::fake_binary(dir.path()),
                data_dir: data,
                credentials_dir: credentials,
                port: Some(free_port()),
                bucket: "nekoguruma".to_owned(),
            },
            dir,
            log: None,
        }
    }

    fn data(&self) -> &Path {
        &self.config.data_dir
    }

    fn credentials(&self) -> &Path {
        &self.config.credentials_dir
    }

    fn secret(&self) -> String {
        std::fs::read_to_string(self.credentials().join(SECRET_KEY_FILE)).unwrap()
    }

    /// The fake's process ID, which it wrote before it did anything else; `None` when it never
    /// ran.
    fn pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.data().join("pid"))
            .ok()
            .and_then(|text| text.trim().parse().ok())
    }

    fn report(&self) -> String {
        std::fs::read_to_string(self.data().join("report.txt")).unwrap_or_default()
    }

    /// The names in the credentials directory.
    fn credential_names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.credentials())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Starts the child on a port that is free at that moment, with a new port when another
    /// process takes it (before the spawn, or while the child binds it). The port that was used
    /// is kept in `config`.
    async fn start(
        &mut self,
        ready_ms: u64,
        stop_ms: u64,
    ) -> Result<ObjectServer, ObjectServerError> {
        let tuning = tuning(ready_ms, stop_ms);
        let (config, log) = (&self.config, &self.log);
        let tried = std::cell::Cell::new(false);
        let mark = log.as_ref().map_or(0, Captured::mark);
        let (result, port) = tokio::time::timeout(
            LIMIT,
            retry_on_lost_port(
                |port| {
                    // Only a retry forgets, and only the dead child's forwarded output: the
                    // server's own lines and those of earlier starts stay.
                    if let (Some(log), true) = (log, tried.replace(true)) {
                        log.forget_forwarded_after(mark);
                    }
                    let mut config = config.clone();
                    config.port = Some(port);
                    let tuning = tuning.clone();
                    async move { ObjectServer::start(&config, 1, &tuning).await }
                },
                object_port_lost,
            ),
        )
        .await
        .expect("the start must not hang");
        self.config.port = Some(port);
        result
    }

    /// A `Config` for `Server::start` that serves on `http_port` with this fixture's child.
    fn server_config(&self, pg: &TestPostgres, http_port: u16) -> Config {
        let mut database = Database::new(pg.url());
        database.connect_timeout_secs = 5;
        Config {
            profile: Profile::Local,
            http: Http {
                port: http_port,
                shutdown_grace_secs: 2,
            },
            database,
            instruction_key: InstructionKeyConfig {
                path: pg.key_path(),
            },
            object_server: Some(self.config.clone()),
        }
    }
}

/// The store's bounds: the defaults, with a short retry pause.
fn store_tuning() -> S3Tuning {
    S3Tuning {
        retry_interval: Duration::from_millis(20),
        ..S3Tuning::default()
    }
}

fn tuning(ready_ms: u64, stop_ms: u64) -> Tuning {
    Tuning {
        ready_timeout: Duration::from_millis(ready_ms),
        stop_timeout: Duration::from_millis(stop_ms),
        fail_after_spawn: false,
    }
}

/// Whether the process has ended: gone, or a zombie that nobody has reaped yet.
fn ended(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            let state = stat.rsplit(')').next().unwrap_or("").trim_start();
            return state.starts_with('Z');
        }
        // No /proc (macOS): signal 0 only checks that the process exists.
        // SAFETY: signal 0 sends nothing.
        unsafe { libc::kill(pid as libc::pid_t, 0) != 0 }
    }
    #[cfg(not(unix))]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .expect("run tasklist");
        !String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    }
}

async fn wait_ended(pid: u32, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !ended(pid) {
        assert!(Instant::now() < deadline, "process {pid} did not end");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Waits until the log holds `needle`.
async fn log_has(log: &Captured, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log.text().contains(needle) {
        assert!(
            Instant::now() < deadline,
            "the log never held {needle:?}:\n{}",
            log.text()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// How many sessions of the server's pool the database still holds.
async fn pool_connections(pg: &TestPostgres) -> String {
    pg.scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name = 'ngr-server'")
        .await
}

/// `start_object_server` without the port that was used.
async fn start_at(
    config: &ObjectServerConfig,
    tuning: &Tuning,
) -> Result<ObjectServer, ObjectServerError> {
    start_object_server(config, tuning).await.0
}

fn refused(result: Result<ObjectServer, ObjectServerError>) -> ObjectServerError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("the start must be refused"),
    }
}

// ---------------------------------------------------------------------------------------------
// C6, C7, C9, C10: what is checked before a child exists

/// C6: a binary that is missing, a directory or not executable is refused before anything is
/// created: no credentials, no lock, no child.
#[tokio::test]
async fn a_binary_that_cannot_run_is_refused_before_any_file_is_made() {
    let fx = Fixture::new("");
    let mut config = fx.config.clone();
    let not_data = fx.dir.path().join("data-never-made");
    config.data_dir = not_data.clone();
    config.binary = fx.dir.path().join("missing");
    let try_start = |config: &ObjectServerConfig| {
        let config = config.clone();
        async move { start_at(&config, &tuning(2000, 500)).await }
    };
    assert!(matches!(
        refused(try_start(&config).await),
        ObjectServerError::BinaryIo {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    ));
    config.binary = fx.dir.path().to_owned();
    assert!(matches!(
        refused(try_start(&config).await),
        ObjectServerError::BinaryNotAFile
    ));
    #[cfg(unix)]
    {
        let plain = fx.dir.path().join("not-executable");
        std::fs::write(&plain, b"x").unwrap();
        // Written owner-only whatever the umask, so that it is the missing execute bit that is
        // refused and not a writable file.
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        config.binary = plain;
        assert!(matches!(
            refused(try_start(&config).await),
            ObjectServerError::BinaryNotExecutable
        ));
    }
    assert!(!not_data.exists(), "the data directory was made");
    assert!(fx.credential_names().is_empty(), "credentials were made");
    assert_eq!(fx.pid(), None);
}

/// C6: a file that is no program of this machine (the wrong architecture, here a text file) ends
/// the start at once and leaves nothing running or locked. The C library's `execvp` hands such a
/// file to the shell, so what the start reports is the end of that shell before it was ready (or,
/// where the exec itself fails, the spawn's `ErrorKind`).
#[cfg(unix)]
#[tokio::test]
async fn a_file_that_is_no_program_ends_the_start_at_once_and_leaves_nothing_running() {
    use std::os::unix::fs::PermissionsExt;
    let mut fx = Fixture::new("");
    let mut config = fx.config.clone();
    let bogus = fx.dir.path().join("bogus");
    std::fs::write(&bogus, b"this is not a program\n").unwrap();
    std::fs::set_permissions(&bogus, std::fs::Permissions::from_mode(0o700)).unwrap();
    private(fx.dir.path());
    config.binary = bogus;
    let began = Instant::now();
    let error = refused(start_at(&config, &tuning(20_000, 500)).await);
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert!(
        matches!(
            error,
            ObjectServerError::Spawn { .. } | ObjectServerError::ExitedBeforeReady(_)
        ),
        "{error}"
    );
    assert_eq!(fx.pid(), None);
    // The lock was released: a start with the real binary works on the same directory.
    let server = fx.start(5000, 1000).await.unwrap();
    server.stop(std::future::pending()).await;
}

/// C7: a binary whose directory group or others can write is refused before spawn; a symbolic
/// link to a good binary is followed and runs.
#[cfg(unix)]
#[tokio::test]
async fn a_writable_binary_directory_is_refused_and_a_symbolic_link_is_followed() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new("");
    let open = fx.dir.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o770)).unwrap();
    let copy = open.join("fake");
    std::fs::copy(crate::support::fake_binary(fx.dir.path()), &copy).unwrap();
    let mut config = fx.config.clone();
    config.binary = copy;
    let error = refused(start_at(&config, &tuning(2000, 500)).await);
    assert!(
        matches!(error, ObjectServerError::BinaryWritable),
        "{error}"
    );
    assert_eq!(fx.pid(), None);
    assert!(fx.credential_names().is_empty());

    let link = fx.dir.path().join("link");
    std::os::unix::fs::symlink(crate::support::fake_binary(fx.dir.path()), &link).unwrap();
    config.binary = link;
    let server = start_at(&config, &tuning(5000, 1000)).await.unwrap();
    assert!(fx.pid().is_some());
    server.stop(std::future::pending()).await;
}

/// C9: an absent data directory is created owner-only when its parent exists; an absent parent,
/// a file and (Unix) a group-writable directory are refused.
#[tokio::test]
async fn the_data_directory_is_created_owner_only_or_refused() {
    let fx = Fixture::new("");
    let mut config = fx.config.clone();
    let fresh = fx.dir.path().join("fresh");
    config.data_dir = fresh.clone();
    let server = start_at(&config, &tuning(5000, 1000)).await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&fresh), 0o700);
        assert_eq!(mode(&fresh.join("volume")), 0o700);
    }
    server.stop(std::future::pending()).await;

    config.data_dir = fx.dir.path().join("no-parent").join("data");
    assert!(matches!(
        refused(start_at(&config, &tuning(2000, 500)).await),
        ObjectServerError::DataDirIo {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    ));
    let file = fx.dir.path().join("data-is-a-file");
    std::fs::write(&file, b"x").unwrap();
    config.data_dir = file;
    assert!(matches!(
        refused(start_at(&config, &tuning(2000, 500)).await),
        ObjectServerError::DataDirNotADirectory
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let open = fx.dir.path().join("group-writable");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o770)).unwrap();
        config.data_dir = open;
        assert!(matches!(
            refused(start_at(&config, &tuning(2000, 500)).await),
            ObjectServerError::DataDirWritable
        ));
    }
}

/// C10: a port that is taken is found by the server's own bind check, which names the key; no
/// child is spawned.
#[tokio::test]
async fn a_taken_object_port_is_refused_by_name_before_the_spawn() {
    let mut fx = Fixture::new("");
    // The test holds the port itself, so that it is taken whatever other processes do.
    let holder = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    fx.config.port = Some(holder.local_addr().unwrap().port());
    let error = refused(ObjectServer::start(&fx.config, 1, &tuning(2000, 500)).await);
    assert!(
        matches!(error, ObjectServerError::PortTaken { port } if port == fx.config.port.unwrap()),
        "{error}"
    );
    assert!(error.to_string().contains("object_server.port"));
    assert_eq!(fx.pid(), None);
    drop(holder);
    fx.start(5000, 1000)
        .await
        .unwrap()
        .stop(std::future::pending())
        .await;
}

/// R10: a second server on the same data directory finds the lock in use before it spawns
/// anything, and of two that start together one serves (K8).
#[tokio::test]
async fn a_second_server_on_one_data_directory_is_refused_before_the_spawn() {
    let mut fx = Fixture::new("");
    let first = fx.start(5000, 1000).await.unwrap();
    let pid = fx.pid().unwrap();
    let other = fx.config.clone();
    let error = refused(start_at(&other, &tuning(2000, 500)).await);
    assert!(matches!(error, ObjectServerError::DataDirInUse), "{error}");
    assert_eq!(fx.pid(), Some(pid), "the second start spawned a child");
    first.stop(std::future::pending()).await;

    // Two first starts at once: one serves, the other is refused, and both files are one pair.
    // (K8, the link race of the credential files themselves, is covered by the 16-thread unit
    // test in `object_server/credentials.rs`; here the data directory's lock decides first.)
    let fx = Fixture::new("");
    let other = fx.config.clone();
    let bounds = tuning(5000, 1000);
    let (a, b) = tokio::join!(start_at(&fx.config, &bounds), start_at(&other, &bounds),);
    let (served, refused) = match (a, b) {
        (Ok(server), Err(error)) | (Err(error), Ok(server)) => (server, error),
        _ => panic!("exactly one of two starts on one directory must serve"),
    };
    assert!(
        matches!(refused, ObjectServerError::DataDirInUse),
        "{refused}"
    );
    assert_eq!(served.credentials().expose_secret(), fx.secret());
    served.stop(std::future::pending()).await;
}

// ---------------------------------------------------------------------------------------------
// K1, K2: the credential files at a start

/// A start that loses its port once (the fake ends with the lost-port status before it binds)
/// is tried again; the log keeps the server's own lines of the first attempt (the credential
/// files it made) and only the live child's forwarded output.
#[tokio::test]
async fn a_retry_after_a_lost_port_keeps_the_servers_lines_and_the_final_childs_output() {
    let (log, _guard) = capture();
    let mut fx = Fixture::new("lose_port_once 1\nprint_secret 1\n");
    fx.log = Some(log.clone());
    let server = fx.start(5000, 1000).await.unwrap();
    assert!(
        fx.data().join("lost-port-once").exists(),
        "the first attempt did not lose its port"
    );
    assert_eq!(server.port(), fx.config.port.unwrap());
    log_has(&log, "echo again").await;
    let text = log.text();
    assert_eq!(
        text.matches("credential files created").count(),
        1,
        "{text}"
    );
    let forwarded = text.lines().filter(|l| l.contains("echo again")).count();
    assert_eq!(forwarded, 1, "{text}");
    server.stop(std::future::pending()).await;
}

/// The fake ends with status 98 when its port is taken (the status the retries look for), and
/// refuses 98 as a status of its own, so that an end on purpose never looks like a lost port.
#[test]
fn the_fake_ends_with_the_lost_port_status_and_reserves_it() {
    let dir = tempfile::tempdir().unwrap();
    let run = |cfg: &str, address: String| {
        let work = dir.path().join("work");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("fake-object-server.cfg"), cfg).unwrap();
        std::process::Command::new(env!("CARGO_BIN_EXE_ngr-fake-object-server"))
            .current_dir(&work)
            .env("RUSTFS_ADDRESS", address)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .code()
    };
    let holder = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let taken = holder.local_addr().unwrap().to_string();
    assert_eq!(run("", taken.clone()), Some(98));
    assert_eq!(run("mode exit\ncode 98\n", taken.clone()), Some(2));
    assert_eq!(run("die_code 98\n", taken), Some(2));
    assert_eq!(
        run("mode exit\ncode 7\n", "127.0.0.1:1".to_owned()),
        Some(7)
    );
}

#[tokio::test]
async fn the_first_start_creates_the_credentials_and_the_next_loads_them_unchanged() {
    let (log, _guard) = capture();
    let mut fx = Fixture::new("");
    fx.log = Some(log.clone());
    let first = fx.start(5000, 1000).await.unwrap();
    let names = fx.credential_names();
    assert_eq!(names, [ACCESS_KEY_FILE, SECRET_KEY_FILE]);
    let secret = fx.secret();
    let access = std::fs::read_to_string(fx.credentials().join(ACCESS_KEY_FILE)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in &names {
            let mode = std::fs::metadata(fx.credentials().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
    }
    first.stop(std::future::pending()).await;
    let text = log.text();
    assert!(
        text.contains("object server credential files created"),
        "{text}"
    );
    assert!(!text.contains(&secret) && !text.contains(&access), "{text}");

    let second = fx.start(5000, 1000).await.unwrap();
    assert_eq!(second.credentials().expose_secret(), secret);
    assert_eq!(fx.secret(), secret);
    assert_eq!(
        std::fs::read_to_string(fx.credentials().join(ACCESS_KEY_FILE)).unwrap(),
        access
    );
    second.stop(std::future::pending()).await;
    assert_eq!(
        log.text().matches("credential files created").count(),
        1,
        "the second start wrote nothing"
    );
}

/// A corrupt credential file ends the start before a spawn and is not overwritten (K3, S2).
#[tokio::test]
async fn a_corrupt_credential_file_ends_the_start_before_the_spawn() {
    let mut fx = Fixture::new("");
    let path = fx.credentials().join(SECRET_KEY_FILE);
    std::fs::write(&path, b"truncated").unwrap();
    private(&path);
    let error = refused(fx.start(2000, 500).await);
    assert!(
        matches!(error, ObjectServerError::Credentials(_)),
        "{error}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"truncated");
    assert_eq!(fx.pid(), None);
}

// ---------------------------------------------------------------------------------------------
// Q1-Q4: readiness

#[tokio::test]
async fn a_server_that_is_ready_in_time_is_started_and_answers_on_loopback() {
    let (log, _guard) = capture();
    let mut fx = Fixture::new("");
    fx.log = Some(log.clone());
    let server = fx.start(5000, 1000).await.unwrap();
    assert_eq!(server.port(), fx.config.port.unwrap());
    assert!(server.pid().is_some());
    server.stop(std::future::pending()).await;
    assert!(log.text().contains("object server ready"), "{}", log.text());
}

/// Q2: a child that is never ready is stopped and reaped, and the error names the last probe.
#[tokio::test]
async fn a_server_that_never_becomes_ready_is_stopped_and_the_start_names_the_last_probe() {
    // On Windows a connect to a port nothing listens on takes about two seconds to be refused,
    // so a probe made before the fake listens is cut by the one-second bound ("cannot connect
    // (timed out)"); the bound is longer there so that a later probe reaches the fake.
    let ready_ms: u64 = if cfg!(windows) { 4000 } else { 1000 };
    for (cfg, last) in [
        ("mode never-ready\n", "status 503"),
        ("mode garbage\n", "unparseable answer"),
        ("mode silent\n", "cannot connect"),
    ] {
        let mut fx = Fixture::new(cfg);
        let began = Instant::now();
        let error = refused(fx.start(ready_ms, 500).await);
        let took = began.elapsed();
        match &error {
            ObjectServerError::NotReady { last: seen, .. } => {
                assert!(seen.contains(last), "{cfg}: {error}");
            }
            other => panic!("{cfg}: {other}"),
        }
        assert!(
            took >= Duration::from_millis(ready_ms - 100),
            "{cfg}: {took:?}"
        );
        assert!(
            took < Duration::from_millis(ready_ms) + Duration::from_secs(4),
            "{cfg}: {took:?}"
        );
        wait_ended(fx.pid().unwrap(), Duration::from_secs(5)).await;
    }
}

/// Q2: a child that ignores SIGTERM is killed after the stop bound, so the start fails only
/// after the ready bound plus the stop bound.
#[cfg(unix)]
#[tokio::test]
async fn a_child_that_ignores_sigterm_is_killed_when_the_start_fails() {
    let (log, _guard) = capture();
    let mut fx = Fixture::new("mode never-ready\nignore_term 1\n");
    let began = Instant::now();
    let error = refused(fx.start(500, 700).await);
    assert!(
        matches!(error, ObjectServerError::NotReady { .. }),
        "{error}"
    );
    assert!(
        began.elapsed() >= Duration::from_millis(1100),
        "{:?}",
        began.elapsed()
    );
    wait_ended(fx.pid().unwrap(), Duration::from_secs(5)).await;
    assert!(
        log.text().contains("did not stop in time"),
        "{}",
        log.text()
    );
}

/// Q3: a child that ends before it is ready ends the start at once, with how it ended.
#[tokio::test]
async fn a_child_that_exits_early_ends_the_start_at_once() {
    let mut fx = Fixture::new("mode exit\ncode 7\n");
    let began = Instant::now();
    let error = refused(fx.start(20_000, 1000).await);
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    match &error {
        ObjectServerError::ExitedBeforeReady(how) => {
            assert!(how.contains('7'), "{how}");
        }
        other => panic!("{other}"),
    }
}

/// Q4: a child that is ready just under the bound is accepted; a probe that is never answered
/// is cut at one second, so the start ends at the bound and not much later.
#[tokio::test]
async fn readiness_just_under_the_bound_is_accepted_and_a_hung_probe_is_cut() {
    let mut fx = Fixture::new("ready_after_ms 1500\n");
    let server = fx.start(3000, 1000).await.unwrap();
    server.stop(std::future::pending()).await;

    let mut fx = Fixture::new("mode hang\n");
    let began = Instant::now();
    let error = refused(fx.start(2500, 500).await);
    let took = began.elapsed();
    assert!(
        matches!(&error, ObjectServerError::NotReady { last, .. } if last.contains("no answer within 1 s")),
        "{error}"
    );
    assert!(took >= Duration::from_millis(2400), "{took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
}

// ---------------------------------------------------------------------------------------------
// S3, S5: guards

/// S3: a failure once the child is ready (the test hook) stops and reaps it: when the start
/// returns, its `/proc` entry is gone, not a zombie that nobody waited for (Linux).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_failure_after_the_spawn_kills_and_reaps_the_child() {
    let fx = Fixture::new("");
    let mut hooked = tuning(5000, 1000);
    hooked.fail_after_spawn = true;
    let error = refused(start_at(&fx.config, &hooked).await);
    assert!(matches!(error, ObjectServerError::Setup(_)), "{error}");
    let pid = fx.pid().expect("the child was ready, so it wrote its pid");
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "the child was not reaped"
    );
}

/// S5: dropping the handle, or the start future, kills the child.
#[tokio::test]
async fn dropping_the_handle_or_the_start_future_kills_the_child() {
    let mut fx = Fixture::new("");
    let server = fx.start(5000, 1000).await.unwrap();
    let pid = server.pid().unwrap();
    assert!(!ended(pid));
    drop(server);
    wait_ended(pid, Duration::from_secs(5)).await;

    let fx = Fixture::new("ready_after_ms 20000\n");
    let early = tokio::time::timeout(
        Duration::from_millis(600),
        start_at(&fx.config, &tuning(30_000, 1000)),
    )
    .await;
    assert!(early.is_err(), "the start must still be waiting");
    wait_ended(fx.pid().unwrap(), Duration::from_secs(5)).await;
}

/// R6: the child must not die when the thread that started it ends: the spawn is on a thread
/// of its own that lives as long as the handle (on Linux the parent-death signal is bound to the
/// thread that spawned).
#[cfg(target_os = "linux")]
#[test]
fn the_child_outlives_the_thread_that_started_it() {
    let fx = Fixture::new("");
    let config = fx.config.clone();
    let starter = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(start_at(&config, &tuning(5000, 1000)))
            .unwrap()
    });
    let server = starter.join().unwrap();
    std::thread::sleep(Duration::from_millis(700));
    let pid = fx.pid().unwrap();
    assert!(
        !ended(pid),
        "the child ended with the thread that started it"
    );
    // SAFETY: kills the test's own fake child.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    // The runtime that owned the handle is gone; leaking it keeps its drop out of the test.
    std::mem::forget(server);
}

// ---------------------------------------------------------------------------------------------
// R7, R8, R9, R11, L1, L6: what the child inherits and what it says

#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_child_gets_a_rebuilt_environment_a_group_of_its_own_and_no_extra_descriptors() {
    let mut fx = Fixture::new("report 1\n");
    let server = fx.start(5000, 1000).await.unwrap();
    let report = fx.report();
    let port = fx.config.port.unwrap();
    // The arguments: the subcommand only.
    let args: Vec<&str> = report
        .lines()
        .filter_map(|l| l.strip_prefix("arg "))
        .collect();
    assert_eq!(args, ["server"]);
    // The environment: exactly what the object server reads and the minimum a process needs.
    let mut keys: Vec<&str> = report
        .lines()
        .filter_map(|l| l.strip_prefix("env "))
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "HOME",
            "PATH",
            "RUSTFS_ACCESS_KEY_FILE",
            "RUSTFS_ADDRESS",
            "RUSTFS_CONSOLE_ENABLE",
            "RUSTFS_SECRET_KEY_FILE",
            "RUSTFS_VOLUMES",
        ]
    );
    // L1: the address is loopback with the configured port.
    assert!(
        report.contains(&format!("env RUSTFS_ADDRESS=127.0.0.1:{port}\n")),
        "{report}"
    );
    let volume = fx.data().join("volume");
    assert!(report.contains(&format!("env RUSTFS_VOLUMES={}\n", volume.display())));
    assert!(report.contains(&format!(
        "env RUSTFS_SECRET_KEY_FILE={}\n",
        fx.credentials().join(SECRET_KEY_FILE).display()
    )));
    // The working directory is the data directory.
    let cwd = report.lines().find_map(|l| l.strip_prefix("cwd ")).unwrap();
    assert_eq!(
        std::fs::canonicalize(cwd).unwrap(),
        std::fs::canonicalize(fx.data()).unwrap()
    );
    // R11: standard input is the null device and no descriptor but 0, 1, 2 (and the one that
    // lists them) is open.
    let fds: Vec<(&str, &str)> = report
        .lines()
        .filter_map(|l| l.strip_prefix("fd "))
        .filter_map(|l| l.split_once(' '))
        .filter(|(_, target)| !target.starts_with("/proc/"))
        .collect();
    assert!(fds.contains(&("0", "/dev/null")), "{fds:?}");
    for (fd, target) in &fds {
        assert!(
            ["0", "1", "2"].contains(fd),
            "an inherited descriptor {fd} -> {target}"
        );
    }
    // R7: its own process group.
    let field = |name: &str| -> i64 {
        report
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    assert_eq!(field("pgid "), field("pid "));
    // SAFETY: plain query of this process's group.
    assert_ne!(field("pgid "), i64::from(unsafe { libc::getpgrp() }));
    assert_eq!(field("ppid "), i64::from(std::process::id()));
    server.stop(std::future::pending()).await;
}

/// R8, R9, L6: the child's output is forwarded per line, bounded and sanitized, with the secret
/// redacted; the secret is in no argument, no environment, no log line and no error.
#[tokio::test]
async fn the_output_is_forwarded_bounded_and_without_the_secret() {
    let (log, _guard) = capture();
    let mut fx = Fixture::new("print_secret 1\nreport 1\n");
    fx.log = Some(log.clone());
    let server = fx.start(5000, 1000).await.unwrap();
    let secret = fx.secret();
    log_has(&log, "echo again").await;
    log_has(&log, "echo is [redacted]").await;
    let text = log.text();
    assert!(!text.contains(&secret), "the secret is in the log");
    // Colour codes are replaced (checked on the forwarded line below), and the 20 000-character
    // line is cut.
    let forwarded: Vec<&str> = text.lines().filter(|l| l.contains("echo again")).collect();
    assert_eq!(forwarded.len(), 1);
    assert!(forwarded[0].len() < 1400, "{}", forwarded[0].len());
    assert!(forwarded[0].contains("?[31m"), "{}", forwarded[0]);
    assert!(forwarded[0].ends_with("..."), "{}", forwarded[0]);
    assert!(
        text.contains("stream=\"stderr\"") || text.contains("stream=stderr"),
        "{text}"
    );
    // The report holds what the child saw: no secret in its arguments or environment.
    assert!(
        !fx.report().contains(&secret),
        "the secret reached the child's report"
    );
    #[cfg(target_os = "linux")]
    {
        let pid = server.pid().unwrap();
        for name in ["cmdline", "environ"] {
            let bytes = std::fs::read(format!("/proc/{pid}/{name}")).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains(&secret),
                "{name} of the child holds the secret"
            );
        }
    }
    // The handle shows no secret either.
    assert!(!format!("{server:?}{:?}", server.credentials()).contains(&secret));
    server.stop(std::future::pending()).await;

    // A failed start's error names no secret.
    let mut fx = Fixture::new("print_secret 1\nmode never-ready\n");
    let error = refused(fx.start(600, 500).await);
    assert!(!format!("{error} {error:?}").contains(&fx.secret()));
}

// ---------------------------------------------------------------------------------------------
// T1-T7, R1-R3, Q5: the end of the child

/// Q5: a child that dies after it was ready is seen by the first wait, and how it ended is
/// reported.
#[tokio::test]
async fn a_child_that_dies_after_the_start_is_seen_by_exited() {
    let mut fx = Fixture::new("die_after_ms 500\ndie_code 4\n");
    let mut server = fx.start(5000, 1000).await.unwrap();
    let how = tokio::time::timeout(Duration::from_secs(5), server.exited())
        .await
        .expect("the end of the child is seen");
    assert!(how.starts_with("exited with") && how.contains('4'), "{how}");
}

/// R2: a child killed by a signal is reported as such.
#[cfg(unix)]
#[tokio::test]
async fn a_child_killed_by_a_signal_is_reported_with_the_signal() {
    let mut fx = Fixture::new("die_after_ms 300\ndie_signal 1\n");
    let mut server = fx.start(5000, 1000).await.unwrap();
    let how = tokio::time::timeout(Duration::from_secs(5), server.exited())
        .await
        .unwrap();
    assert!(how.contains("signal") && how.contains('9'), "{how}");
}

/// T2: a child that ignores SIGTERM is killed after the stop bound and reaped.
#[cfg(unix)]
#[tokio::test]
async fn a_child_that_ignores_sigterm_is_killed_after_the_stop_bound() {
    let (log, _guard) = capture();
    let mut fx = Fixture::new("ignore_term 1\n");
    fx.log = Some(log.clone());
    let server = fx.start(5000, 800).await.unwrap();
    let pid = server.pid().unwrap();
    let began = Instant::now();
    server.stop(std::future::pending()).await;
    let took = began.elapsed();
    assert!(took >= Duration::from_millis(750), "{took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert!(ended(pid));
    assert!(
        log.text().contains("did not stop in time"),
        "{}",
        log.text()
    );
}

/// T1, T3: the stop sequence ends a child that obeys SIGTERM at once, and a second stop signal
/// (the `force` future) kills one that does not, without waiting out the bound.
#[cfg(unix)]
#[tokio::test]
async fn a_term_ends_the_child_at_once_and_a_second_signal_kills_it() {
    let mut fx = Fixture::new("");
    let server = fx.start(5000, 8000).await.unwrap();
    let pid = server.pid().unwrap();
    let began = Instant::now();
    server.stop(std::future::pending()).await;
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "{:?}",
        began.elapsed()
    );
    assert!(ended(pid));

    let mut fx = Fixture::new("ignore_term 1\n");
    let server = fx.start(5000, 20_000).await.unwrap();
    let pid = server.pid().unwrap();
    let began = Instant::now();
    server
        .stop(async { tokio::time::sleep(Duration::from_millis(300)).await })
        .await;
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert!(ended(pid));
}

/// T4: a child that is gone already is only reaped.
#[tokio::test]
async fn a_child_that_is_gone_already_is_only_reaped() {
    let (log, _guard) = capture();
    let mut fx = Fixture::new("die_after_ms 300\n");
    fx.log = Some(log.clone());
    let server = fx.start(5000, 8000).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let began = Instant::now();
    server.stop(std::future::pending()).await;
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "{:?}",
        began.elapsed()
    );
    assert!(log.text().contains("had ended already"), "{}", log.text());
}

/// T6: a wait that fails is logged and the stop still returns (the child's status was taken by
/// another waiter, so the wait fails).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_failing_wait_is_logged_and_the_stop_returns() {
    let (log, _guard) = capture();
    let mut fx = Fixture::new("");
    fx.log = Some(log.clone());
    let server = fx.start(5000, 800).await.unwrap();
    let pid = server.pid().unwrap() as libc::pid_t;
    // SAFETY: kills and reaps the test's own fake child, so that the server's wait finds no child.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        let mut status = 0;
        libc::waitpid(pid, &mut status, 0);
    }
    tokio::time::timeout(Duration::from_secs(15), server.stop(std::future::pending()))
        .await
        .expect("the stop returns");
    assert!(log.text().contains("ERROR"), "{}", log.text());
}

/// T7: the forwarders end at the end of the stream; one that a grandchild in a group of its own
/// keeps open is aborted after one second.
#[cfg(unix)]
#[tokio::test]
async fn a_forwarder_held_open_by_a_grandchild_is_aborted_after_a_second() {
    let mut fx = Fixture::new("grandchild group\n");
    let server = fx.start(5000, 1000).await.unwrap();
    let began = Instant::now();
    server.stop(std::future::pending()).await;
    let took = began.elapsed();
    assert!(took >= Duration::from_millis(900), "{took:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
    // Kill what the test left behind.
    if let Some(pid) = std::fs::read_to_string(fx.data().join("grandchild.pid"))
        .ok()
        .and_then(|t| t.trim().parse::<libc::pid_t>().ok())
    {
        // SAFETY: kills the test's own sleeper.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

/// A grandchild in the child's own group does not outlive the stop: the group is killed after the
/// reap, so the pipes close and nothing is left.
#[cfg(unix)]
#[tokio::test]
async fn a_grandchild_in_the_childs_group_is_killed_with_it() {
    let mut fx = Fixture::new("grandchild same\n");
    let server = fx.start(5000, 1000).await.unwrap();
    let began = Instant::now();
    server.stop(std::future::pending()).await;
    assert!(
        began.elapsed() < Duration::from_millis(900),
        "{:?}",
        began.elapsed()
    );
    let pid: u32 = std::fs::read_to_string(fx.data().join("grandchild.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_ended(pid, Duration::from_secs(2)).await;
}

/// Codex review: a child that ended and was reaped before the stop still has its group killed,
/// since the group's ID is kept from the spawn.
#[cfg(unix)]
#[tokio::test]
async fn a_grandchild_outlives_no_stop_after_its_parent_was_reaped() {
    let mut fx = Fixture::new("grandchild same\ndie_after_ms 100\n");
    let mut server = fx.start(5000, 1000).await.unwrap();
    // The child ends after its first ready answer; `exited` reaps it.
    let how = tokio::time::timeout(Duration::from_secs(5), server.exited())
        .await
        .expect("the child ends");
    assert!(!how.is_empty());
    let pid: u32 = std::fs::read_to_string(fx.data().join("grandchild.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    server.stop(std::future::pending()).await;
    wait_ended(pid, Duration::from_secs(2)).await;
}

// ---------------------------------------------------------------------------------------------
// With the server: S1, S2, S4, S7, C11, C1, R1, R3, T1, T3

fn started(result: Result<Server, StartError>) -> Server {
    result.expect("the server must start")
}

fn failed(result: Result<Server, StartError>) -> StartError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("the start must fail"),
    }
}

/// `Server::start_tuned` with a fresh object-server port for each attempt when the port was
/// lost.
async fn start_tuned(
    config: &Config,
    object: Tuning,
    store: S3Tuning,
) -> Result<Server, StartError> {
    retry_on_lost_port(
        |port| {
            let mut config = config.clone();
            if let Some(object_server) = config.object_server.as_mut() {
                object_server.port = Some(port);
            }
            let (object, store) = (object.clone(), store.clone());
            async move { Server::start_tuned(&config, object, store).await }
        },
        |error, port| {
            matches!(error, StartError::ObjectServer(error) if object_port_lost(error, port))
        },
    )
    .await
    .0
}

async fn start_server(
    fx: &Fixture,
    pg: &TestPostgres,
    http_port: u16,
) -> Result<Server, StartError> {
    tokio::time::timeout(
        LIMIT,
        start_tuned(
            &fx.server_config(pg, http_port),
            tuning(5000, 1000),
            store_tuning(),
        ),
    )
    .await
    .expect("Server::start must not hang")
}

/// C1: a `Config` built in code without `[object_server]` starts no child and no lock.
#[tokio::test]
async fn a_config_without_an_object_server_starts_none() {
    let pg = TestPostgres::start();
    let fx = Fixture::new("");
    let mut config = fx.server_config(&pg, 0);
    config.object_server = None;
    let server = started(Server::start(&config).await);
    assert!(server.object_server().is_none());
    server.run(async {}, std::future::pending()).await.unwrap();
    assert_eq!(fx.pid(), None);
    assert!(fx.credential_names().is_empty());
}

/// S1: a failure before step 5 (here the instruction key) leaves no credentials, lock or child.
#[tokio::test]
async fn a_failure_before_the_object_server_leaves_no_credentials_lock_or_child() {
    let pg = TestPostgres::start();
    let fx = Fixture::new("");
    let mut config = fx.server_config(&pg, 0);
    config.instruction_key.path = fx
        .dir
        .path()
        .join("no-such-directory")
        .join("instruction.key");
    let error = failed(start_tuned(&config, tuning(5000, 1000), store_tuning()).await);
    assert!(matches!(error, StartError::InstructionKey(_)), "{error}");
    assert!(fx.credential_names().is_empty());
    assert_eq!(fx.pid(), None);
    assert!(!fx.data().join(".lock").exists());
    assert_eq!(pool_connections(&pg).await, "0");
}

/// S2: a failure in step 5 before the spawn closes the pool and leaves no child.
#[tokio::test]
async fn a_failure_in_step_five_before_the_spawn_closes_the_pool() {
    let pg = TestPostgres::start();
    let mut fx = Fixture::new("");
    fx.config.binary = fx.dir.path().join("missing");
    let error = failed(start_server(&fx, &pg, 0).await);
    assert!(matches!(error, StartError::ObjectServer(_)), "{error}");
    assert_eq!(fx.pid(), None);
    assert_eq!(pool_connections(&pg).await, "0");
}

/// S4: the object store failing to open after the child is ready (the bucket check is refused)
/// stops and reaps the child, releases its data directory and closes the pool.
#[tokio::test]
async fn a_store_that_fails_to_open_stops_the_child_and_closes_the_pool() {
    let pg = TestPostgres::start();
    let fx = Fixture::new("s3_status 403\n");
    let config = fx.server_config(&pg, 0);
    let error = failed(start_tuned(&config, tuning(5000, 1000), store_tuning()).await);
    assert!(
        matches!(error, StartError::Storage(StoreError::Refused { .. })),
        "{error}"
    );
    wait_ended(fx.pid().unwrap(), Duration::from_secs(5)).await;
    assert_eq!(pool_connections(&pg).await, "0");
    // The data directory's lock is free again.
    std::fs::write(fx.data().join("fake-object-server.cfg"), "").unwrap();
    drop(started(start_server(&fx, &pg, 0).await));
}

/// S4: the child ending while the store retries ends the start at once, naming the child.
#[tokio::test]
async fn a_child_that_ends_while_the_store_opens_ends_the_start() {
    let pg = TestPostgres::start();
    let fx = Fixture::new("s3_status 503\ndie_after_ms 300\ndie_code 7\n");
    let config = fx.server_config(&pg, 0);
    let mut tune = store_tuning();
    tune.ready_timeout = Duration::from_secs(20);
    let began = Instant::now();
    let error = failed(start_tuned(&config, tuning(5000, 1000), tune).await);
    assert!(
        matches!(
            error,
            StartError::ObjectServer(ObjectServerError::ExitedBeforeReady(_))
        ),
        "{error}"
    );
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(pool_connections(&pg).await, "0");
}

/// A bucket check that is refused by a 5xx is retried until the budget is spent, then fails with
/// the status.
#[tokio::test]
async fn a_store_that_never_gets_a_bucket_fails_when_the_budget_is_spent() {
    let pg = TestPostgres::start();
    let fx = Fixture::new("s3_status 503\n");
    let config = fx.server_config(&pg, 0);
    let mut tune = store_tuning();
    tune.ready_timeout = Duration::from_millis(400);
    let error = failed(start_tuned(&config, tuning(5000, 1000), tune).await);
    assert!(
        matches!(
            error,
            StartError::Storage(StoreError::Server { status: 503, .. })
        ),
        "{error}"
    );
    assert_eq!(pool_connections(&pg).await, "0");
}

/// C11: the server's own port taken stops the child and closes the pool.
#[tokio::test]
async fn a_taken_http_port_stops_the_child_and_closes_the_pool() {
    let pg = TestPostgres::start();
    let fx = Fixture::new("");
    let holder = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let taken = holder.local_addr().unwrap().port();
    let error = failed(start_server(&fx, &pg, taken).await);
    assert!(matches!(error, StartError::Bind { .. }), "{error}");
    wait_ended(fx.pid().unwrap(), Duration::from_secs(5)).await;
    assert_eq!(pool_connections(&pg).await, "0");
}

/// S5: a `Server` dropped without `run` kills the child.
#[tokio::test]
async fn a_server_dropped_without_run_kills_the_child() {
    let pg = TestPostgres::start();
    let fx = Fixture::new("");
    let server = started(start_server(&fx, &pg, 0).await);
    let pid = server.object_server().unwrap().pid().unwrap();
    drop(server);
    wait_ended(pid, Duration::from_secs(5)).await;
}

/// R1: the child ending before any stop signal makes `run` stop as for a signal and fail; the
/// child is reaped and the pool closed.
#[tokio::test]
async fn a_child_that_ends_while_the_server_runs_ends_the_run_with_an_error() {
    let pg = TestPostgres::start();
    let fx = Fixture::new("die_after_ms 600\ndie_code 4\n");
    let server = started(start_server(&fx, &pg, 0).await);
    let result = tokio::time::timeout(
        LIMIT,
        server.run(std::future::pending(), std::future::pending()),
    )
    .await
    .expect("the run ends");
    match result {
        Err(RunError::ObjectServerExited(how)) => assert!(how.contains('4'), "{how}"),
        other => panic!("the run must fail with the child's end: {other:?}"),
    }
    assert_eq!(pool_connections(&pg).await, "0");
}

/// R3: a stop signal together with the child's end is a stop (exit 0), not a failure. The
/// select between the two is biased towards the stop, so the outcome does not depend on luck;
/// 32 rounds guard against a change to a random choice (a coin flip passes them all with
/// probability 2^-32).
#[tokio::test]
async fn a_stop_signal_together_with_the_childs_end_is_a_clean_stop() {
    let pg = TestPostgres::start();
    for round in 0..32 {
        let fx = Fixture::new("die_after_ms 450\n");
        let server = started(start_server(&fx, &pg, 0).await);
        let pid = server.object_server().unwrap().pid().unwrap();
        wait_ended(pid, Duration::from_secs(5)).await;
        let result = tokio::time::timeout(LIMIT, server.run(async {}, std::future::pending()))
            .await
            .expect("the run ends");
        assert!(result.is_ok(), "round {round}: {result:?}");
    }
}

/// T1, T3: a stop signal ends the child with the server and a second signal kills a child that
/// ignores SIGTERM at once, both when it arrives during the grace period and after it.
#[cfg(unix)]
#[tokio::test]
async fn a_stop_ends_the_child_and_a_second_signal_kills_it_at_once() {
    let (log, _guard) = capture();
    let pg = TestPostgres::start();
    // The first signal alone: the child obeys SIGTERM.
    let fx = Fixture::new("");
    let server = started(start_server(&fx, &pg, 0).await);
    let pid = server.object_server().unwrap().pid().unwrap();
    server.run(async {}, std::future::pending()).await.unwrap();
    assert!(ended(pid));
    assert_eq!(pool_connections(&pg).await, "0");
    // `stop` ran (a drop would log nothing): the child obeyed SIGTERM with exit status 0.
    let text = log.text();
    assert!(
        text.lines()
            .any(|l| l.contains("object server stopped") && l.contains("exit status: 0")),
        "{text}"
    );

    // The child ignores SIGTERM; the second signal comes after the grace wait.
    let fx = Fixture::new("ignore_term 1\n");
    let config = fx.server_config(&pg, 0);
    let server = started(start_tuned(&config, tuning(5000, 20_000), store_tuning()).await);
    let pid = server.object_server().unwrap().pid().unwrap();
    let began = Instant::now();
    tokio::time::timeout(
        LIMIT,
        server.run(async {}, async {
            tokio::time::sleep(Duration::from_millis(300)).await
        }),
    )
    .await
    .expect("the run ends")
    .unwrap();
    // The force future was waited for (a drop would be at once) and then ended the wait.
    assert!(
        began.elapsed() >= Duration::from_millis(300),
        "{:?}",
        began.elapsed()
    );
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "{:?}",
        began.elapsed()
    );
    assert!(ended(pid));
    assert_eq!(
        log.text()
            .matches("second stop signal; killing the object server")
            .count(),
        1,
        "{}",
        log.text()
    );

    // The second signal comes during the grace period, while a connection that sends nothing
    // keeps it open.
    let fx = Fixture::new("ignore_term 1\n");
    let mut config = fx.server_config(&pg, 0);
    config.http.shutdown_grace_secs = 30;
    let server = started(start_tuned(&config, tuning(5000, 20_000), store_tuning()).await);
    let pid = server.object_server().unwrap().pid().unwrap();
    let addr = server.local_addr();
    let _idle = tokio::net::TcpStream::connect(addr).await.unwrap();
    let began = Instant::now();
    tokio::time::timeout(
        LIMIT,
        server.run(
            async { tokio::time::sleep(Duration::from_millis(200)).await },
            async { tokio::time::sleep(Duration::from_millis(900)).await },
        ),
    )
    .await
    .expect("the run ends")
    .unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "{:?}",
        began.elapsed()
    );
    assert!(ended(pid));
    assert_eq!(
        log.text()
            .matches("second stop signal; killing the object server")
            .count(),
        2,
        "{}",
        log.text()
    );
}

// ---------------------------------------------------------------------------------------------
// The server as a process: S6, S7, S8, R5

fn steer(dir: &Path, cfg: &str) -> PathBuf {
    let data = dir.join("object-server");
    std::fs::create_dir_all(&data).unwrap();
    private(&data);
    std::fs::write(data.join("fake-object-server.cfg"), cfg).unwrap();
    data
}

/// Whether something accepts a connection on the loopback port within 300 ms.
fn listening(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
        Duration::from_millis(300),
    )
    .is_ok()
}

#[cfg(target_os = "linux")]
fn read_pid(data: &Path, limit: Duration) -> u32 {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(pid) = std::fs::read_to_string(data.join("pid"))
            .ok()
            .and_then(|t| t.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "the object server never started");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "linux")]
fn ended_within(pid: u32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if ended(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    ended(pid)
}

/// S7: the commands never read or create credentials, never lock the data directory and never
/// spawn the child, whatever the database says.
#[test]
fn the_commands_never_touch_the_object_server() {
    let pg = TestPostgres::start();
    let dir = tempfile::tempdir().unwrap();
    let data = steer(dir.path(), "");
    let path = crate::binary::write_config(dir.path(), free_port(), &pg.url());
    let run = |args: &[&str]| {
        let mut command = crate::binary::server(args);
        command.stderr(std::process::Stdio::piped());
        crate::binary::wait_output(command.spawn().unwrap(), Duration::from_secs(60))
    };
    let config = path.to_str().unwrap();
    run(&[
        "registration-code",
        "--config",
        config,
        "--owner",
        "alice",
        "--name",
        "Bench PC",
        "--agent-key",
        &"ab".repeat(32),
    ]);
    run(&[
        "user",
        "disable",
        "--config",
        config,
        "--username",
        "nobody",
    ]);
    assert!(
        !data.join("pid").exists(),
        "a command spawned the object server"
    );
    assert!(
        !data.join(".lock").exists(),
        "a command locked the data directory"
    );
    assert!(!data.join("volume").exists());
    let credentials = dir.path().join("credentials");
    assert_eq!(std::fs::read_dir(credentials).unwrap().count(), 0);
}

/// A server that ended before it served: checks that it ended with exit 1 and a log that shows a
/// lost port (and that attempts are left), else fails with the status and the log's tail.
/// Returns true.
fn lost_port_end(status: std::process::ExitStatus, log_path: &Path, attempt: usize) -> bool {
    let log = std::fs::read_to_string(log_path).unwrap_or_default();
    assert!(
        status.code() == Some(1) && crate::support::log_shows_lost_port(&log),
        "the server ended unexpectedly ({status}):\n{}",
        crate::binary::log_tail(&log)
    );
    assert!(
        attempt < crate::support::PORT_ATTEMPTS,
        "a port was lost in all {attempt} attempts; the last log:\n{}",
        crate::binary::log_tail(&log)
    );
    eprintln!("a port was lost (attempt {attempt}); trying new ports");
    true
}

/// S8: the log line that says the server listens comes after the object server's readiness, and
/// nothing listens on the server's port before it.
#[test]
fn the_server_listens_only_after_the_object_server_is_ready() {
    let pg = TestPostgres::start();
    let dir = tempfile::tempdir().unwrap();
    let data = steer(dir.path(), "ready_after_ms 3000\n");
    // The ports are only known to be free a moment before the server binds them. A server that
    // loses one of them ends with exit 1 before it listens, and is started again on new ports.
    let mut attempt = 0;
    let (mut child, log_path) = loop {
        attempt += 1;
        let _ = std::fs::remove_file(data.join("pid"));
        let port = free_port();
        let path = crate::binary::write_config(dir.path(), port, &pg.url());
        let log_path = dir.path().join(format!("server-{attempt}.log"));
        let mut command = crate::binary::server(&["--config", path.to_str().unwrap()]);
        command.stderr(std::fs::File::create(&log_path).unwrap());
        let mut child = crate::binary::KillOnDrop::new(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut lost = false;
        let started = loop {
            if data.join("pid").exists() {
                break true;
            }
            if let Some(status) = child.status() {
                lost = lost_port_end(status, &log_path, attempt);
                break false;
            }
            assert!(Instant::now() < deadline, "the object server never started");
            std::thread::sleep(Duration::from_millis(20));
        };
        if !started {
            assert!(lost);
            continue;
        }
        // Not `get_health`: a connect to a closed port takes about two seconds to be refused on
        // Windows, long enough for the server to get ready and bind meanwhile. This check
        // assumes that nothing else binds the released port in that window.
        assert!(
            !listening(port),
            "the server listened before the object server was ready"
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let served = loop {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            if log.contains("listening") && crate::binary::get_health(port).is_some() {
                break true;
            }
            if let Some(status) = child.status() {
                lost_port_end(status, &log_path, attempt);
                break false;
            }
            assert!(Instant::now() < deadline, "the server never listened");
            std::thread::sleep(Duration::from_millis(50));
        };
        if served {
            break (child, log_path);
        }
    };
    child.stop_with_sigterm();
    let log = std::fs::read_to_string(&log_path).unwrap();
    let ready = log.find("object server ready").expect("the readiness line");
    let listening = log.find("listening").expect("the listening line");
    assert!(ready < listening, "{log}");
}

/// S6: a stop signal during the start (the signal handlers are not installed yet) ends the
/// server by the default disposition, and the child ends with it (Linux: parent-death signal).
#[cfg(target_os = "linux")]
#[test]
fn a_stop_signal_during_the_start_ends_the_child_with_the_server() {
    let pg = TestPostgres::start();
    let dir = tempfile::tempdir().unwrap();
    let data = steer(dir.path(), "ready_after_ms 25000\n");
    // The fake writes its pid before it binds its port, so the start is taken as under way once
    // the server has forwarded the fake's "up" line; a server that ends first lost a port and is
    // started again with new ones.
    let mut attempt = 0;
    let (mut child, pid) = loop {
        attempt += 1;
        let path = crate::binary::write_config(dir.path(), free_port(), &pg.url());
        let log_path = dir.path().join(format!("server-{attempt}.log"));
        let mut command = crate::binary::server(&["--config", path.to_str().unwrap()]);
        command.stderr(std::fs::File::create(&log_path).unwrap());
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let up = loop {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            if log.contains("fake object server up") {
                break true;
            }
            if let Some(status) = child.try_wait().unwrap() {
                lost_port_end(status, &log_path, attempt);
                break false;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("the object server never came up");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if up {
            let pid = read_pid(&data, Duration::from_secs(30));
            break (child, pid);
        }
    };
    // SAFETY: signals the test's own child.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let status = crate::binary::wait_exit(&mut child, Duration::from_secs(20));
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(libc::SIGTERM),
        "the default disposition ends it"
    );
    assert!(
        ended_within(pid, Duration::from_secs(5)),
        "the object server outlived the server"
    );
}

/// R5: a server that is killed (SIGKILL) takes the child with it (Linux: parent-death signal).
#[cfg(target_os = "linux")]
#[test]
fn a_killed_server_takes_the_child_with_it() {
    let pg = TestPostgres::start();
    let dir = tempfile::tempdir().unwrap();
    let data = steer(dir.path(), "");
    let (mut child, _port, _) =
        crate::binary::start_server_with(dir.path(), &pg, Duration::from_secs(30), |_| {});
    let pid = read_pid(&data, Duration::from_secs(30));
    assert!(!ended(pid));
    child.kill().unwrap();
    let _ = child.wait();
    assert!(
        ended_within(pid, Duration::from_secs(5)),
        "the object server outlived the server"
    );
}

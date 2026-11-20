//! A throwaway PostgreSQL per test, started from the machine's own `initdb` and `pg_ctl`
//! (ADR-458). A missing PostgreSQL fails the test; it never skips it.
//!
//! The binaries are looked up in this order: the directory `NGR_TEST_PG_BIN`, the first `initdb`
//! on `PATH`, the highest `/usr/lib/postgresql/<version>/bin`, then the directory `PGBIN` (set on
//! the Windows runner image). The cluster lives in a temporary directory, accepts password
//! authentication only, listens on 127.0.0.1 on a free port and no Unix socket, and is stopped
//! when the value is dropped. When the tests run as root, PostgreSQL refuses to start, so the
//! helper runs it as the `postgres` operating-system account (Unix only), with the temporary
//! directory handed to that account; the directory's parents must be searchable by it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use server::config::ObjectServerConfig;
use server::object_server::{ObjectServer, ObjectServerError, Tuning};
use sqlx::Connection;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgSslMode};

const EXE: &str = std::env::consts::EXE_SUFFIX;

/// A running PostgreSQL cluster that belongs to one test.
pub struct TestPostgres {
    dir: tempfile::TempDir,
    /// A directory for the server's instruction key file, which the server account owns (the
    /// cluster's own directory belongs to the `postgres` account when the tests run as root).
    key_dir: tempfile::TempDir,
    bin: PathBuf,
    port: u16,
    password: String,
    /// The `postgres` account's ids when the tests run as root (Unix only).
    #[cfg(unix)]
    run_as: Option<(u32, u32)>,
    running: bool,
}

impl TestPostgres {
    /// Creates and starts a cluster. Panics, naming what it looked for, when PostgreSQL is
    /// missing or does not start.
    pub fn start() -> TestPostgres {
        let bin = find_bin();
        #[cfg(unix)]
        let run_as = root_identity();
        let dir = tempfile::Builder::new()
            .prefix("ngr-test-pg-")
            .tempdir()
            .expect("a temporary directory");
        #[cfg(unix)]
        if let Some((uid, gid)) = run_as {
            std::os::unix::fs::chown(dir.path(), Some(uid), Some(gid))
                .expect("hand the temporary directory to the postgres account");
        }
        // Owner-only whatever the umask: the server refuses a group-writable key directory.
        let key_dir = tempfile::tempdir().expect("a temporary directory for the key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(key_dir.path(), std::fs::Permissions::from_mode(0o700))
                .expect("make the key directory owner-only");
        }
        let mut pg = TestPostgres {
            dir,
            key_dir,
            bin,
            port: 0,
            password: random_password(),
            #[cfg(unix)]
            run_as,
            running: false,
        };
        pg.init();
        // A free port is only known to be free for a moment; try again when another process
        // takes it before the server binds.
        let mut last = String::new();
        for _attempt in 0..5 {
            pg.port = free_port();
            pg.write_settings();
            match pg.pg_ctl(&["start", "-w", "-t", "60", "-l"], Some(&pg.log_path())) {
                Ok(()) => {
                    pg.running = true;
                    return pg;
                }
                Err(message) => last = message,
            }
        }
        panic!("PostgreSQL did not start: {last}\n{}", pg.log_tail());
    }

    /// The instruction key path of a server started against this cluster: tests that start
    /// several servers on one cluster share it, as a deployment's servers do.
    pub fn key_path(&self) -> PathBuf {
        self.key_dir.path().join("instruction.key")
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn log_path(&self) -> PathBuf {
        self.dir.path().join("postgres.log")
    }

    fn log_tail(&self) -> String {
        let text = std::fs::read_to_string(self.log_path()).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let from = lines.len().saturating_sub(30);
        format!("postgres.log:\n{}", lines[from..].join("\n"))
    }

    /// A command that runs `tool` from the PostgreSQL directory, as the `postgres` account when
    /// the tests run as root, in the cluster's directory, with its standard streams closed (a
    /// postmaster that inherited the test's pipes would hold them open).
    fn command(&self, tool: &str) -> Command {
        let mut c = Command::new(self.bin.join(format!("{tool}{EXE}")));
        c.current_dir(self.dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        if let Some((uid, gid)) = self.run_as {
            use std::os::unix::process::CommandExt;
            c.uid(uid).gid(gid);
        }
        c
    }

    fn init(&self) {
        let pwfile = self.dir.path().join("pwfile");
        std::fs::write(&pwfile, &self.password).expect("write the password file");
        #[cfg(unix)]
        if let Some((uid, gid)) = self.run_as {
            std::os::unix::fs::chown(&pwfile, Some(uid), Some(gid)).expect("chown the pwfile");
        }
        let out = self
            .command("initdb")
            .arg("-D")
            .arg(self.data())
            .args(["-N", "-U", "postgres", "-A", "scram-sha-256", "-E", "UTF8"])
            .arg("--locale=C")
            .arg(format!("--pwfile={}", pwfile.display()))
            .output()
            .unwrap_or_else(|e| panic!("cannot run initdb: {e}"));
        assert!(
            out.status.success(),
            "initdb failed ({}); when the tests run as root, the `postgres` account must be able \
             to enter every parent of {} (TMPDIR)\n{}",
            out.status,
            self.dir.path().display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Settings go into the file and not into `pg_ctl -o`, which is quoted differently on
    /// Windows. The listener is TCP on 127.0.0.1 only: an empty `unix_socket_directories` (the
    /// default directory is not writable by an ordinary user).
    fn write_settings(&self) {
        let conf = self.data().join("postgresql.conf");
        let base = std::fs::read_to_string(&conf).expect("read postgresql.conf");
        let base = base
            .split("\n# ngr-test\n")
            .next()
            .unwrap_or(&base)
            .to_owned();
        let settings = format!(
            "{base}\n# ngr-test\nlisten_addresses = '127.0.0.1'\nport = {}\n\
             unix_socket_directories = ''\nfsync = off\nshared_buffers = 16MB\n\
             max_connections = 30\n",
            self.port
        );
        std::fs::write(&conf, settings).expect("write postgresql.conf");
    }

    /// Runs `pg_ctl` with `args` (and the data directory); `log` is passed after a trailing
    /// `-l`.
    fn pg_ctl(&self, args: &[&str], log: Option<&Path>) -> Result<(), String> {
        let mut c = self.command("pg_ctl");
        // Never a pipe: `pg_ctl start` leaves the postmaster running, and a child that inherited
        // a pipe would hold it open, so waiting for the pipe to end would hang (on Windows it
        // does). What the postmaster says goes to the log file, which the failure reports.
        c.stderr(Stdio::null());
        c.args(args);
        if let Some(log) = log {
            c.arg(log);
        }
        c.arg("-D").arg(self.data());
        let status = c.status().map_err(|e| format!("cannot run pg_ctl: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "pg_ctl {args:?} failed ({status})\n{}",
                self.log_tail()
            ))
        }
    }

    /// Stops the cluster (a database that is down), waiting until it has stopped.
    pub fn stop(&mut self) {
        if self.running {
            self.pg_ctl(&["stop", "-w", "-m", "fast"], None)
                .expect("stop PostgreSQL");
            self.running = false;
        }
    }

    /// The URL of the cluster's `postgres` database as its superuser.
    pub fn url(&self) -> String {
        format!(
            "postgres://postgres:{}@127.0.0.1:{}/postgres",
            self.password, self.port
        )
    }

    /// Connection options as the superuser, with every part set here so that `PG*` variables
    /// and `~/.pgpass` on a developer's machine cannot change them (`PGOPTIONS` aside, which
    /// sqlx offers no way to unset).
    fn options(&self) -> PgConnectOptions {
        PgConnectOptions::new_without_pgpass()
            .host("127.0.0.1")
            .port(self.port)
            .username("postgres")
            .password(&self.password)
            .database("postgres")
            .ssl_mode(PgSslMode::Disable)
    }

    /// A pool of its own on the cluster, as the superuser; each call is a new set of sessions.
    pub fn pool(&self) -> sqlx::PgPool {
        server::db::pool_lazy(self.options(), std::time::Duration::from_secs(5))
    }

    /// Runs `sql` (a fixed statement without parameters) on a new connection, as the superuser.
    pub async fn execute(&self, sql: &'static str) {
        let options = self.options();
        let mut conn = PgConnection::connect_with(&options)
            .await
            .expect("connect to the test database");
        sqlx::query(sql)
            .execute(&mut conn)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    /// The text of the first column of the first row of `sql`, as the superuser.
    pub async fn scalar(&self, sql: &str) -> String {
        let options = self.options();
        let mut conn = PgConnection::connect_with(&options)
            .await
            .expect("connect to the test database");
        sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
            "SELECT COALESCE(({sql})::text, '')"
        )))
        .fetch_one(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }
}

impl Drop for TestPostgres {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("{}", self.log_tail());
        }
        if self.running {
            let _ = self.pg_ctl(&["stop", "-w", "-m", "immediate"], None);
        }
    }
}

/// A loopback port that nothing listens on. The socket is bound and never listens, and it holds
/// the port for as long as the value lives, so no other test process can take it and every
/// connection to it is refused. A port bound and then released could be taken meanwhile. Keep
/// the value for as long as the test uses the port.
pub struct ClosedPort(tokio::net::TcpSocket);

impl ClosedPort {
    pub fn new() -> Self {
        let socket = tokio::net::TcpSocket::new_v4().expect("a socket");
        socket
            .bind((std::net::Ipv4Addr::LOCALHOST, 0).into())
            .expect("a loopback port");
        Self(socket)
    }

    pub fn port(&self) -> u16 {
        self.0.local_addr().expect("a bound socket").port()
    }
}

/// The URL of a PostgreSQL that is not there: a port that nothing listens on, held by the
/// returned guard, which the caller keeps while the URL is in use.
pub fn refused_url(password: &str) -> (ClosedPort, String) {
    let closed = ClosedPort::new();
    let url = format!(
        "postgres://ngr:{password}@127.0.0.1:{}/nekoguruma",
        closed.port()
    );
    (closed, url)
}

/// The `[object_server]` table of a configuration file in `dir`: the fake object server
/// (`src/bin/ngr-fake-object-server.rs`), a data directory `object-server` and a credentials
/// directory `credentials` (made here) in `dir`, and a free port. A literal string, since a
/// Windows path holds backslashes.
pub fn object_server_table(dir: &Path) -> String {
    let credentials = dir.join("credentials");
    std::fs::create_dir_all(&credentials).expect("a credentials directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Owner-only whatever the umask: the server refuses a group-writable directory.
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    format!(
        "[object_server]\nbinary = '{}'\ndata_dir = '{}'\ncredentials_dir = '{}'\nport = {}\n",
        fake_binary(dir).display(),
        dir.join("object-server").display(),
        credentials.display(),
        free_port()
    )
}

/// A private copy of the fake object server inside `dir`: the server refuses a binary that
/// group or others can write, and a build under a umask of 002 leaves the file (and the directory
/// Cargo builds into) group-writable, so the copy, and `dir` (a temporary directory of the test,
/// which a umask of 002 also leaves group-writable), are made owner-only.
pub fn fake_binary(dir: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_BIN_EXE_ngr-fake-object-server"));
    let target = dir.join(format!("ngr-fake-object-server{EXE}"));
    if !target.exists() {
        std::fs::copy(source, &target).expect("copy the fake object server");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [target.as_path(), dir] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
    }
    target
}

/// How many ports a start that binds a picked port in a child process tries.
pub const PORT_ATTEMPTS: usize = 5;

/// The status with which the fake object server ends when its bind finds the address in use
/// (`src/bin/ngr-fake-object-server.rs`, which refuses it as a status of its own modes): a lost
/// port, told from a child that ends on purpose.
pub const LOST_PORT_EXIT: u8 = 98;

/// Runs `start` with a port that was free a moment ago, and again with a fresh port when it fails
/// with an error that `lost` (given the error and the port) calls the loss of the port to another
/// process, at most `PORT_ATTEMPTS` times in all. Returns the last result and the port it used.
/// For a child process that binds the port itself: a port picked and released is only free for a
/// moment. Each retry is printed, so that it shows in the test's output.
pub async fn retry_on_lost_port<T, E, Fut>(
    mut start: impl FnMut(u16) -> Fut,
    lost: impl Fn(&E, u16) -> bool,
) -> (Result<T, E>, u16)
where
    E: std::fmt::Display,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut attempt = 1;
    loop {
        let port = free_port();
        let result = start(port).await;
        match &result {
            Err(error) if attempt < PORT_ATTEMPTS && lost(error, port) => {
                eprintln!("port {port} was lost (attempt {attempt}): {error}; trying a new port");
                attempt += 1;
            }
            _ => return (result, port),
        }
    }
}

/// The object server's own check found the port taken.
pub fn object_port_taken(error: &ObjectServerError) -> bool {
    matches!(error, ObjectServerError::PortTaken { .. })
}

/// Whether a rendered exit status is the fake's lost-port status (`exit status: 98` on Unix,
/// `exit code: 98` on Windows).
fn is_lost_port_status(how: &str) -> bool {
    how.ends_with(&format!(": {LOST_PORT_EXIT}"))
}

/// Whether something accepts a connection on the loopback port now.
fn answers(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
}

/// The port was lost to another process: taken before the spawn (`PortTaken`), or the child ended
/// because its bind failed. The fake says so with `LOST_PORT_EXIT`. The real object server ends
/// with status 1 and an "Address in use" line that the start's error does not carry, so for any
/// other end of the child the port counts as lost when something answers on it now (the child
/// itself never got ready, so it is not the one that does).
pub fn object_port_lost(error: &ObjectServerError, port: u16) -> bool {
    object_port_taken(error)
        || matches!(error, ObjectServerError::ExitedBeforeReady(how)
            if is_lost_port_status(how) || answers(port))
}

/// Whether the log of an `ngr-server` process shows that a port it needed was lost: its own
/// port in use, the object server's port in use, the fake object server ending with
/// `LOST_PORT_EXIT`, or the real one's forwarded "Address in use" line.
pub fn log_shows_lost_port(log: &str) -> bool {
    log.contains("cannot listen on")
        || log.contains("object_server.port) is in use")
        || log.lines().any(|line| {
            (line.contains("ended before it was ready")
                && (line.contains(&format!("status: {LOST_PORT_EXIT}"))
                    || line.contains(&format!("code: {LOST_PORT_EXIT}"))))
                || (line.contains("object server output") && line.contains("Address in use"))
        })
}

/// `ObjectServer::start` on `config` with a fresh port for each attempt (`retry_on_lost_port`).
/// Returns the result and the last port tried.
pub async fn start_object_server(
    config: &ObjectServerConfig,
    tuning: &Tuning,
) -> (Result<ObjectServer, ObjectServerError>, u16) {
    retry_on_lost_port(
        |port| {
            let mut config = config.clone();
            config.port = Some(port);
            async move { ObjectServer::start(&config, 1, tuning).await }
        },
        object_port_lost,
    )
    .await
}

#[test]
fn a_lost_port_is_told_from_an_end_on_purpose() {
    let ended = |how: &str| ObjectServerError::ExitedBeforeReady(how.to_owned());
    let silent = ClosedPort::new().port();
    assert!(object_port_lost(
        &ObjectServerError::PortTaken { port: 1 },
        silent
    ));
    assert!(object_port_lost(
        &ended("exited with exit status: 98"),
        silent
    ));
    assert!(object_port_lost(
        &ended("exited with exit code: 98"),
        silent
    ));
    assert!(!object_port_lost(
        &ended("exited with exit status: 7"),
        silent
    ));
    assert!(!object_port_lost(
        &ended("exited with exit status: 198"),
        silent
    ));
    // Something answers on the port: the child's end was a lost port whatever its status.
    let holder = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = holder.local_addr().unwrap().port();
    assert!(object_port_lost(&ended("exited with exit status: 1"), port));

    assert!(log_shows_lost_port(
        "ERROR cannot listen on 127.0.0.1:1: in use"
    ));
    assert!(log_shows_lost_port(
        "127.0.0.1:2 (object_server.port) is in use; if"
    ));
    assert!(log_shows_lost_port(
        "2026-11-20T09:59:58Z the object server ended before it was ready: exited with exit status: 98"
    ));
    assert!(log_shows_lost_port(
        "the object server ended before it was ready: exited with exit code: 98"
    ));
    assert!(log_shows_lost_port(
        "INFO object server output stream=\"stderr\" line=Server runtime failed: Address in use (os error 98)"
    ));
    assert!(!log_shows_lost_port(
        "2026-11-20T09:59:98Z the object server ended before it was ready: exited with exit status: 7"
    ));
    assert!(!log_shows_lost_port("the object server refused the bucket"));
}

/// A port that was free a moment ago. Another process may take it before it is used, so a caller
/// that hands it to a child process or to PostgreSQL retries with a new one (`TestPostgres::start`,
/// `retry_on_lost_port`); a server started in the test's own process binds port 0 instead. A
/// caller that needs a port nothing listens on uses it only where no one binds it.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind a free port")
        .local_addr()
        .expect("the bound address")
        .port()
}

/// 32 random hex digits.
pub fn random_password() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("random bytes");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn has_tools(dir: &Path) -> bool {
    dir.join(format!("initdb{EXE}")).is_file() && dir.join(format!("pg_ctl{EXE}")).is_file()
}

/// The directory with `initdb` and `pg_ctl`, by the lookup order of the module's documentation.
fn find_bin() -> PathBuf {
    if let Some(dir) = std::env::var_os("NGR_TEST_PG_BIN") {
        let dir = PathBuf::from(dir);
        assert!(
            has_tools(&dir),
            "NGR_TEST_PG_BIN is {} but it holds no initdb and pg_ctl",
            dir.display()
        );
        return dir;
    }
    if let Some(path) = std::env::var_os("PATH")
        && let Some(dir) = std::env::split_paths(&path).find(|d| has_tools(d))
    {
        return dir;
    }
    if let Some(dir) = highest_installed() {
        return dir;
    }
    if let Some(dir) = std::env::var_os("PGBIN") {
        let dir = PathBuf::from(dir);
        if has_tools(&dir) {
            return dir;
        }
    }
    panic!(
        "no PostgreSQL found: the server's tests start a throwaway one and need initdb and \
         pg_ctl (looked in NGR_TEST_PG_BIN, PATH, /usr/lib/postgresql/*/bin and PGBIN); install \
         PostgreSQL or point NGR_TEST_PG_BIN at its bin directory"
    );
}

/// The `bin` directory of the highest major version under `/usr/lib/postgresql` (Debian and
/// Ubuntu).
fn highest_installed() -> Option<PathBuf> {
    let mut versions: Vec<(u32, PathBuf)> = std::fs::read_dir("/usr/lib/postgresql")
        .ok()?
        .filter_map(|e| {
            let e = e.ok()?;
            let major = e.file_name().to_str()?.parse::<u32>().ok()?;
            Some((major, e.path().join("bin")))
        })
        .filter(|(_, bin)| has_tools(bin))
        .collect();
    versions.sort();
    versions.pop().map(|(_, bin)| bin)
}

/// `Some((uid, gid))` of the `postgres` account when the tests run as root, since PostgreSQL
/// refuses to run as root; `None` otherwise. The ids come from the `id` command, which keeps the
/// helper free of `unsafe` and of a libc binding.
#[cfg(unix)]
fn root_identity() -> Option<(u32, u32)> {
    let id = |args: &[&str]| -> Option<u32> {
        let out = Command::new("id").args(args).output().ok()?;
        String::from_utf8(out.stdout).ok()?.trim().parse().ok()
    };
    if id(&["-u"])? != 0 {
        return None;
    }
    let uid = id(&["-u", "postgres"]);
    let gid = id(&["-g", "postgres"]);
    match (uid, gid) {
        (Some(u), Some(g)) => Some((u, g)),
        _ => panic!(
            "the tests run as root, which PostgreSQL refuses, and there is no `postgres` account \
             to run it as: run the tests as an ordinary user"
        ),
    }
}

/// Keeps one dispatcher registered for the life of the test process, so that a tracing callsite's
/// interest is never decided by the thread that happens to reach it first. With exactly one
/// dispatcher in existence, tracing takes a new callsite's interest from the default dispatcher
/// of the reaching thread; a thread with no subscriber then caches "never" for the callsite, and a
/// test that captures the log on another thread sees nothing. A second, silent dispatcher makes
/// tracing combine the interests of all of them. Call it before installing a capturing subscriber.
pub(crate) fn pin_dispatchers() {
    static PIN: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
    PIN.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
}

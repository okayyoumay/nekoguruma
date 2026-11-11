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

use sqlx::Connection;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgSslMode};

const EXE: &str = std::env::consts::EXE_SUFFIX;

/// A running PostgreSQL cluster that belongs to one test.
pub struct TestPostgres {
    dir: tempfile::TempDir,
    bin: PathBuf,
    port: u16,
    password: String,
    run_as: Option<(u32, u32)>,
    running: bool,
}

impl TestPostgres {
    /// Creates and starts a cluster. Panics, naming what it looked for, when PostgreSQL is
    /// missing or does not start.
    pub fn start() -> TestPostgres {
        let bin = find_bin();
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
        let mut pg = TestPostgres {
            dir,
            bin,
            port: 0,
            password: random_password(),
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

/// The URL of a PostgreSQL that is not there: a port that nothing listens on.
pub fn refused_url(password: &str) -> String {
    format!(
        "postgres://ngr:{password}@127.0.0.1:{}/nekoguruma",
        free_port()
    )
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind a free port")
        .local_addr()
        .expect("the bound address")
        .port()
}

fn random_password() -> String {
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
fn root_identity() -> Option<(u32, u32)> {
    if !cfg!(unix) {
        return None;
    }
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

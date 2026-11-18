//! The bundled S3-compatible object server (design 13, 13.3; ADR-491): the pinned RustFS binary
//! that the server starts as its child, stops with itself and holds to loopback.
//!
//! [`ObjectServer::start`] is step 5 of the start order (ADR-458, ADR-462, ADR-474), after the
//! instruction key and before the object store opens:
//!
//! 1. the port is resolved, then the binary is checked (nothing else exists yet);
//! 2. the data directory is created if absent, checked and locked (`.lock`, the object store's
//!    lock code), and its `volume` directory, which the object server needs to exist, is created;
//! 3. the credential files are read or created ([`credentials`]);
//! 4. the port is tried on `127.0.0.1` (a taken port names `object_server.port`);
//! 5. the child is spawned: a cleared and rebuilt environment, standard input null, output piped
//!    and forwarded line by line to the log ([`forward`]), the data directory as its working
//!    directory, its own process group on Unix, no console on Windows;
//! 6. the server waits for `GET /health/ready` on the loopback port to answer 200, at most
//!    [`READY_TIMEOUT`], each probe at most [`PROBE_TIMEOUT`], and for no longer than the child
//!    lives.
//!
//! Every failure after the spawn runs the one stop sequence ([`ObjectServer::stop`]): SIGTERM
//! (Windows: terminate), a wait of [`STOP_TIMEOUT`], SIGKILL, the reap. The child handle is
//! killed when dropped, and the operating system ends the child with the server whenever the
//! server ends without running that sequence: on Linux by the parent-death signal, set on a thread
//! that lives as long as the [`ObjectServer`] (the signal is bound to the thread that spawned the
//! child), on Windows by the kill-on-close job object the server joined before spawning
//! ([`job`]). Other Unix systems have neither: a child can outlive a killed server, and the next
//! start fails at step 4 with a hint.
//!
//! The listener is the object server's: `RUSTFS_ADDRESS` is `127.0.0.1:<port>` and no
//! configuration key changes it. The credentials are given as the paths of their two files
//! (`RUSTFS_ACCESS_KEY_FILE`, `RUSTFS_SECRET_KEY_FILE`), which the object server reads; that is
//! not configuration by environment in the sense of ADR-456 item 7, which is about the server's
//! own settings, and no secret is in the environment, in an argument or in the configuration.
//! Nothing here formats the [`std::process::Command`], whose `Debug` shows its environment.

pub mod credentials;
mod forward;
#[cfg(windows)]
mod job;

use std::future::Future;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use shared_crypto::Zeroizing;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::{ConfigError, ObjectServerConfig};
use crate::fs_guard::{DirectoryFault, check_directory};
use crate::storage::StoreError;
use crate::storage::local::{ProcessLock, create_directory, ensure_area, take_process_lock};
pub use credentials::{Credentials, CredentialsError};

/// How long the server waits for the object server to answer its readiness probe.
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the stop sequence waits for the child after SIGTERM before it kills it.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest one readiness probe may take, connect and answer together.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// The pause between two probes.
const PROBE_INTERVAL: Duration = Duration::from_millis(100);
/// How long the stop sequence waits for the log forwarders after the child ended.
const FORWARDER_JOIN: Duration = Duration::from_secs(1);
/// How long the reap after a kill may take before the failure is logged and given up.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// The name of the directory in the data directory that the object server is given as its volume.
pub const VOLUME_DIR: &str = "volume";

/// The bounds of one start and stop. The defaults are the constants above; tests shorten them.
#[derive(Debug, Clone)]
#[doc(hidden)]
pub struct Tuning {
    pub ready_timeout: Duration,
    pub stop_timeout: Duration,
    /// Makes the step after the spawn fail, to prove that a child is then stopped and reaped.
    pub fail_after_spawn: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            ready_timeout: READY_TIMEOUT,
            stop_timeout: STOP_TIMEOUT,
            fail_after_spawn: false,
        }
    }
}

/// Why the object server could not be made ready. Fixed text and `ErrorKind`s: never a value from
/// a credential file and never the child's command.
#[derive(Debug, thiserror::Error)]
pub enum ObjectServerError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("cannot {op} object_server.binary: {kind}")]
    BinaryIo {
        op: &'static str,
        kind: io::ErrorKind,
    },
    #[error("object_server.binary is not a regular file")]
    BinaryNotAFile,
    #[error("object_server.binary is not executable")]
    BinaryNotExecutable,
    #[error(
        "object_server.binary, or the directory it is in, is writable by group or others; a \
         binary the server starts must be writable by its account only"
    )]
    BinaryWritable,
    #[error(
        "object_server.binary, or the directory it is in, is owned by neither the account the \
         server runs as nor root"
    )]
    BinaryNotOwned,
    #[error("cannot {op} object_server.data_dir: {kind}")]
    DataDirIo {
        op: &'static str,
        kind: io::ErrorKind,
    },
    #[error("object_server.data_dir is not a directory")]
    DataDirNotADirectory,
    #[error("object_server.data_dir is owned by neither the account the server runs as nor root")]
    DataDirNotOwned,
    #[error(
        "object_server.data_dir is writable by group or others; it must be writable by the \
         server's account only"
    )]
    DataDirWritable,
    #[error("another process uses object_server.data_dir (another server on the same directory?)")]
    DataDirInUse,
    #[error(transparent)]
    Credentials(#[from] CredentialsError),
    #[error(
        "127.0.0.1:{port} (object_server.port) is in use; if an object server of an earlier run \
         was left running (a system without a parent-death signal or job object), stop it"
    )]
    PortTaken { port: u16 },
    #[error("cannot start the object server binary: {kind}")]
    Spawn { kind: io::ErrorKind },
    #[error("cannot make the object server end with the server: {0}")]
    Setup(&'static str),
    #[error("the object server ended before it was ready: {0}")]
    ExitedBeforeReady(String),
    #[error(
        "the object server was not ready within {secs} s (last probe: {last}); its output is in \
         the log above"
    )]
    NotReady { secs: u64, last: String },
}

/// A running object server: the child, the locks and credentials it was started with, and the
/// tasks that forward its output.
///
/// Dropping it without [`ObjectServer::stop`] kills the child (the guard of the start's failure
/// paths and of a dropped start future); the operating system does the same if the process dies.
pub struct ObjectServer {
    // Field order is drop order: the child is killed first, then the lock is released, and the
    // spawning thread, whose end is what Linux's parent-death signal is bound to, ends last.
    child: Child,
    port: u16,
    credentials: Credentials,
    forwarders: Vec<JoinHandle<()>>,
    stop_timeout: Duration,
    _lock: ProcessLock,
    _spawner: std::sync::mpsc::Sender<()>,
}

impl std::fmt::Debug for ObjectServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectServer")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl ObjectServer {
    /// Starts the object server and waits until it is ready (the steps in the module's
    /// documentation). `http_port` is the server's own port, for the default of
    /// `config.port`. On an error nothing is left running.
    pub async fn start(
        config: &ObjectServerConfig,
        http_port: u16,
        tuning: &Tuning,
    ) -> Result<ObjectServer, ObjectServerError> {
        let port = config.resolved_port(http_port)?;
        let binary = check_binary(&config.binary)?;
        let (lock, volume) = prepare_data_dir(&config.data_dir)?;
        let credentials = Credentials::load_or_create(&config.credentials_dir)?;
        if credentials.created() {
            tracing::info!("object server credential files created");
        }
        // The port is tried here so that a port that is taken is named; the window between this
        // and the child's own bind is closed by the child ending before it is ready.
        match std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(listener) => drop(listener),
            Err(_) => return Err(ObjectServerError::PortTaken { port }),
        }
        let command = command(&binary, &config.data_dir, &volume, port, &credentials);
        let (child, spawner) = spawn(command).await?;
        let mut server = ObjectServer {
            child,
            port,
            credentials,
            forwarders: Vec::new(),
            stop_timeout: tuning.stop_timeout,
            _lock: lock,
            _spawner: spawner,
        };
        if let Err(error) = server.after_spawn(tuning) {
            server.stop(std::future::pending()).await;
            return Err(error);
        }
        if let Err(error) = server.wait_ready(tuning.ready_timeout).await {
            server.stop(std::future::pending()).await;
            return Err(error);
        }
        tracing::info!(port, "object server ready");
        Ok(server)
    }

    /// Takes the child's pipes into the log forwarders. A failure here, or the test hook, is a
    /// failure after the spawn.
    fn after_spawn(&mut self, tuning: &Tuning) -> Result<(), ObjectServerError> {
        let secret = Arc::new(Zeroizing::new(self.credentials.expose_secret().to_owned()));
        let stdout = self.child.stdout.take();
        let stderr = self.child.stderr.take();
        if let Some(stdout) = stdout {
            self.forwarders
                .push(forward::spawn("stdout", stdout, Arc::clone(&secret)));
        }
        if let Some(stderr) = stderr {
            self.forwarders
                .push(forward::spawn("stderr", stderr, Arc::clone(&secret)));
        }
        if tuning.fail_after_spawn || self.forwarders.len() != 2 {
            return Err(ObjectServerError::Setup("the child's output is not piped"));
        }
        Ok(())
    }

    /// Waits for the readiness probe to answer 200, or for the child to end, or for `timeout`.
    async fn wait_ready(&mut self, timeout: Duration) -> Result<(), ObjectServerError> {
        let deadline = Instant::now() + timeout;
        let mut last = String::from("no probe finished");
        loop {
            let probing = async {
                match probe(self.port).await {
                    Ok(()) => Ok(()),
                    Err(outcome) => {
                        tokio::time::sleep(PROBE_INTERVAL).await;
                        Err(outcome)
                    }
                }
            };
            tokio::select! {
                biased;
                status = self.child.wait() => {
                    return Err(ObjectServerError::ExitedBeforeReady(describe(status)));
                }
                step = tokio::time::timeout_at(deadline, probing) => match step {
                    Ok(Ok(())) => return Ok(()),
                    Ok(Err(outcome)) => last = outcome,
                    Err(_) => {
                        return Err(ObjectServerError::NotReady {
                            secs: timeout.as_secs(),
                            last,
                        });
                    }
                },
            }
        }
    }

    /// The loopback port the object server listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The credentials the object server was started with.
    pub fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    /// The child's process ID while it is not reaped.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Resolves when the child has ended, with how. The one place that waits for the child; it
    /// is cancel-safe, so the serving loop can poll it beside its other events.
    pub async fn exited(&mut self) -> String {
        describe(self.child.wait().await)
    }

    /// The stop sequence, the only one: if the child has not ended it is sent SIGTERM (Windows:
    /// terminated), waited for at most the stop timeout, killed and reaped; `force` resolving (a
    /// second stop signal) skips the wait. Then the forwarders are joined for at most one second
    /// and aborted after that. Failures are logged and do not change the caller's exit code.
    pub async fn stop(mut self, force: impl Future<Output = ()>) {
        stop_child(&mut self.child, self.stop_timeout, force).await;
        for forwarder in std::mem::take(&mut self.forwarders) {
            let abort = forwarder.abort_handle();
            if tokio::time::timeout(FORWARDER_JOIN, forwarder)
                .await
                .is_err()
            {
                abort.abort();
            }
        }
    }
}

/// How a child ended, or why that is not known.
fn describe(status: io::Result<ExitStatus>) -> String {
    match status {
        Ok(status) => format!("exited with {status}"),
        Err(error) => format!("its end could not be read: {}", error.kind()),
    }
}

/// The stop sequence of [`ObjectServer::stop`] on a child.
async fn stop_child(child: &mut Child, stop_timeout: Duration, force: impl Future<Output = ()>) {
    match child.try_wait() {
        Ok(Some(status)) => {
            tracing::info!(%status, "object server had ended already");
            return;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!(kind = ?error.kind(), "cannot read the object server's state");
        }
    }
    terminate(child);
    let mut force = std::pin::pin!(force);
    let ended = tokio::select! {
        status = child.wait() => Some(status),
        () = tokio::time::sleep(stop_timeout) => {
            tracing::warn!(
                secs = stop_timeout.as_secs(),
                "object server did not stop in time; killing it"
            );
            None
        }
        () = &mut force => {
            tracing::warn!("second stop signal; killing the object server");
            None
        }
    };
    match ended {
        Some(Ok(status)) => tracing::info!(%status, "object server stopped"),
        Some(Err(error)) => {
            tracing::error!(kind = ?error.kind(), "cannot wait for the object server");
        }
        None => {
            if let Err(error) = child.start_kill() {
                tracing::error!(kind = ?error.kind(), "cannot kill the object server");
            }
            match tokio::time::timeout(REAP_TIMEOUT, child.wait()).await {
                Ok(Ok(status)) => tracing::info!(%status, "object server killed"),
                Ok(Err(error)) => {
                    tracing::error!(kind = ?error.kind(), "cannot reap the object server");
                }
                Err(_) => tracing::error!("the killed object server was not reaped in time"),
            }
        }
    }
}

/// Asks the child to end: SIGTERM on Unix, termination on Windows (which has no signal a console-less
/// child receives).
fn terminate(child: &mut Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SAFETY: `kill` takes a process ID and a signal number and has no other precondition;
            // the ID is of a child this process has not reaped, so it is not reused.
            if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
                tracing::error!(
                    kind = ?io::Error::last_os_error().kind(),
                    "cannot signal the object server"
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(error) = child.start_kill() {
            tracing::error!(kind = ?error.kind(), "cannot terminate the object server");
        }
    }
}

/// Checks the binary before anything is created (ADR-491): it must name a file, be executable and,
/// on Unix, neither it nor its directory may be writable by group or others or belong to an
/// account other than the server's and root. A symbolic link is followed and the target judged;
/// the returned path is the one to run, so that the file judged is the file started.
fn check_binary(path: &Path) -> Result<PathBuf, ObjectServerError> {
    let io_fault = |op| {
        move |error: io::Error| ObjectServerError::BinaryIo {
            op,
            kind: error.kind(),
        }
    };
    if !path.is_absolute() {
        return Err(ObjectServerError::Config(ConfigError::ObjectServerPath));
    }
    #[cfg(unix)]
    let resolved = std::fs::canonicalize(path).map_err(io_fault("resolve"))?;
    #[cfg(not(unix))]
    let resolved = path.to_owned();
    let meta = std::fs::metadata(&resolved).map_err(io_fault("inspect"))?;
    if !meta.is_file() {
        return Err(ObjectServerError::BinaryNotAFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = resolved.parent().ok_or(ObjectServerError::BinaryNotAFile)?;
        let dir_meta = std::fs::metadata(dir).map_err(io_fault("inspect the directory of"))?;
        if let Some(fault) = binary_fault(
            meta.uid(),
            meta.permissions().mode(),
            dir_meta.uid(),
            dir_meta.permissions().mode(),
        ) {
            return Err(fault);
        }
    }
    Ok(resolved)
}

/// The decision of [`check_binary`] on what the metadata of the file and of its directory say, so
/// that a test can judge an owner it cannot create.
#[cfg(unix)]
fn binary_fault(
    file_uid: u32,
    file_mode: u32,
    dir_uid: u32,
    dir_mode: u32,
) -> Option<ObjectServerError> {
    let euid = crate::fs_guard::euid();
    let trusted = |uid: u32| uid == euid || uid == 0;
    if !trusted(file_uid) || !trusted(dir_uid) {
        return Some(ObjectServerError::BinaryNotOwned);
    }
    if file_mode & 0o022 != 0 || dir_mode & 0o022 != 0 {
        return Some(ObjectServerError::BinaryWritable);
    }
    if file_mode & 0o111 == 0 {
        return Some(ObjectServerError::BinaryNotExecutable);
    }
    None
}

/// The error for a directory the shared check refused, named for the data directory.
fn data_dir_fault(fault: DirectoryFault) -> ObjectServerError {
    match fault {
        DirectoryFault::Io(kind) => ObjectServerError::DataDirIo {
            op: "inspect",
            kind,
        },
        DirectoryFault::NotADirectory => ObjectServerError::DataDirNotADirectory,
        DirectoryFault::NotOwned => ObjectServerError::DataDirNotOwned,
        DirectoryFault::Writable => ObjectServerError::DataDirWritable,
    }
}

fn data_dir_store_error(error: StoreError) -> ObjectServerError {
    match error {
        StoreError::InUse => ObjectServerError::DataDirInUse,
        StoreError::NotADirectory => ObjectServerError::DataDirNotADirectory,
        StoreError::DirectoryNotOwned => ObjectServerError::DataDirNotOwned,
        StoreError::DirectoryWritable => ObjectServerError::DataDirWritable,
        StoreError::Io { op, kind } => ObjectServerError::DataDirIo { op, kind },
        _ => ObjectServerError::DataDirIo {
            op: "use",
            kind: io::ErrorKind::Other,
        },
    }
}

/// Creates the data directory if it is absent (its parent must exist), checks it, locks it and
/// makes the volume directory in it. Returns the lock, which the object server holds for its life,
/// and the volume's path.
fn prepare_data_dir(dir: &Path) -> Result<(ProcessLock, PathBuf), ObjectServerError> {
    match std::fs::metadata(dir) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_directory(dir).map_err(|error| ObjectServerError::DataDirIo {
                op: "create",
                kind: error.kind(),
            })?;
            if let Some(parent) = dir.parent() {
                crate::fs_guard::sync_directory(parent).map_err(|error| {
                    ObjectServerError::DataDirIo {
                        op: "sync the parent of",
                        kind: error.kind(),
                    }
                })?;
            }
        }
        Err(error) => {
            return Err(ObjectServerError::DataDirIo {
                op: "inspect",
                kind: error.kind(),
            });
        }
    }
    check_directory(dir).map_err(data_dir_fault)?;
    let lock = take_process_lock(&dir.join(".lock")).map_err(data_dir_store_error)?;
    let volume = dir.join(VOLUME_DIR);
    ensure_area(&volume).map_err(data_dir_store_error)?;
    Ok((lock, volume))
}

/// The child's command: the pinned binary's `server` subcommand with a cleared environment that
/// holds what the object server reads (the address, the volume, the two credential file paths, the
/// console switched off) and the minimum a process needs, and nothing of the
/// server's own. The secret is not in it: only the paths of the credential files are.
fn command(
    binary: &Path,
    data_dir: &Path,
    volume: &Path,
    port: u16,
    credentials: &Credentials,
) -> std::process::Command {
    let mut command = std::process::Command::new(binary);
    command
        .arg("server")
        .env_clear()
        .env("RUSTFS_ADDRESS", format!("127.0.0.1:{port}"))
        .env("RUSTFS_VOLUMES", volume)
        // RustFS 1.0.6 starts its web console on 0.0.0.0:9001 unless told not to; this server
        // has no use for it, and the only listener the child may have is the S3 one.
        .env("RUSTFS_CONSOLE_ENABLE", "false")
        .env("RUSTFS_ACCESS_KEY_FILE", credentials.access_key_path())
        .env("RUSTFS_SECRET_KEY_FILE", credentials.secret_key_path())
        .current_dir(data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command
            .env("HOME", data_dir)
            .env("PATH", "/usr/bin:/bin")
            // Its own group, so that a Ctrl-C at the terminal reaches the server alone, which
            // orders the stop.
            .process_group(0);
        #[cfg(target_os = "linux")]
        {
            let parent = std::process::id() as libc::pid_t;
            // SAFETY: the closure runs between fork and exec and calls only `prctl`, `getppid`
            // and `_exit`, which are async-signal-safe, and allocates nothing.
            unsafe {
                command.pre_exec(move || {
                    // The signal fires when the thread that spawned the child ends (the spawning
                    // thread outlives the server's use of the child).
                    if libc::prctl(
                        libc::PR_SET_PDEATHSIG,
                        libc::SIGKILL as libc::c_ulong,
                        0,
                        0,
                        0,
                    ) != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    // The parent may have died before the signal was armed.
                    if libc::getppid() != parent {
                        libc::_exit(1);
                    }
                    Ok(())
                });
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console, so a Ctrl-C or Ctrl-Break at the server's console does
        // not reach the child.
        command.creation_flags(0x0800_0000);
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
    }
    command
}

/// Spawns the child on a thread of its own that lives until the returned sender is dropped: on
/// Linux the parent-death signal is bound to the thread that spawned the child, so spawning on a
/// task of the runtime (whose thread may end first) or on the blocking pool would let the signal
/// fire early. On Windows the server first joins its kill-on-close job object.
async fn spawn(
    command: std::process::Command,
) -> Result<(Child, std::sync::mpsc::Sender<()>), ObjectServerError> {
    #[cfg(windows)]
    job::ensure().map_err(|_| ObjectServerError::Setup("cannot create the job object"))?;
    let handle = tokio::runtime::Handle::current();
    let (alive, hold) = std::sync::mpsc::channel::<()>();
    let (spawned, result) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("object-server-spawner".to_owned())
        .spawn(move || {
            let _enter = handle.enter();
            let mut command = tokio::process::Command::from(command);
            command.kill_on_drop(true);
            let _ = spawned.send(command.spawn());
            // Held until the server is done with the child.
            let _ = hold.recv();
        })
        .map_err(|error| ObjectServerError::Spawn { kind: error.kind() })?;
    match result.await {
        Ok(Ok(child)) => Ok((child, alive)),
        Ok(Err(error)) => Err(ObjectServerError::Spawn { kind: error.kind() }),
        Err(_) => Err(ObjectServerError::Setup("the spawning thread ended")),
    }
}

/// One readiness probe: `GET /health/ready` on the loopback port, answered 200 or not. The
/// error is a short description for the log and the start's error.
async fn probe(port: u16) -> Result<(), String> {
    let attempt = async {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|error| format!("cannot connect: {}", error.kind()))?;
        let request = format!(
            "GET /health/ready HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|error| format!("cannot send: {}", error.kind()))?;
        // The status line is all that is read.
        let mut head = Vec::new();
        let mut buf = [0u8; 256];
        while head.len() < 512 && !head.windows(2).any(|w| w == b"\r\n") {
            let n = stream
                .read(&mut buf)
                .await
                .map_err(|error| format!("cannot read: {}", error.kind()))?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        parse_status(&head)
    };
    match tokio::time::timeout(PROBE_TIMEOUT, attempt).await {
        Ok(Ok(200)) => Ok(()),
        Ok(Ok(status)) => Err(format!("status {status}")),
        Ok(Err(outcome)) => Err(outcome),
        Err(_) => Err(format!("no answer within {} s", PROBE_TIMEOUT.as_secs())),
    }
}

/// The status code of an HTTP/1 status line, or why there is none.
fn parse_status(head: &[u8]) -> Result<u16, String> {
    let line = head.split(|&b| b == b'\n').next().unwrap_or_default();
    let text = std::str::from_utf8(line).map_err(|_| "unparseable answer".to_owned())?;
    let mut parts = text.trim_end().splitn(3, ' ');
    match (parts.next(), parts.next()) {
        (Some("HTTP/1.1" | "HTTP/1.0"), Some(code)) if code.len() == 3 => code
            .parse::<u16>()
            .map_err(|_| "unparseable answer".to_owned()),
        _ => Err("unparseable answer".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_line_is_parsed_and_anything_else_is_unparseable() {
        assert_eq!(parse_status(b"HTTP/1.1 200 OK\r\nx: y\r\n"), Ok(200));
        assert_eq!(
            parse_status(b"HTTP/1.0 503 Service Unavailable\r\n"),
            Ok(503)
        );
        assert_eq!(parse_status(b"HTTP/1.1 200\r\n"), Ok(200));
        for bad in [
            &b""[..],
            b"garbage\r\n",
            b"HTTP/2 200 OK\r\n",
            b"HTTP/1.1 20 OK\r\n",
            b"HTTP/1.1 abc OK\r\n",
            b"\xff\xfe\r\n",
        ] {
            assert!(parse_status(bad).is_err(), "{bad:?}");
        }
    }

    // C7: the owner and mode rules on synthetic metadata, since an owner cannot be made here.
    #[cfg(unix)]
    #[test]
    fn a_binary_or_directory_that_others_can_write_or_a_stranger_owns_is_refused() {
        let me = crate::fs_guard::euid();
        let stranger = if me == 4_242_424 {
            4_242_425
        } else {
            4_242_424
        };
        let fault = |f_uid, f_mode, d_uid, d_mode| binary_fault(f_uid, f_mode, d_uid, d_mode);
        assert!(fault(me, 0o755, me, 0o755).is_none());
        assert!(fault(0, 0o755, 0, 0o755).is_none());
        assert!(fault(me, 0o500, me, 0o700).is_none());
        for mode in [0o775, 0o757, 0o777, 0o722] {
            assert!(
                matches!(
                    fault(me, mode, me, 0o755),
                    Some(ObjectServerError::BinaryWritable)
                ),
                "file {mode:o}"
            );
            assert!(
                matches!(
                    fault(me, 0o755, me, mode),
                    Some(ObjectServerError::BinaryWritable)
                ),
                "dir {mode:o}"
            );
        }
        assert!(matches!(
            fault(stranger, 0o755, me, 0o755),
            Some(ObjectServerError::BinaryNotOwned)
        ));
        assert!(matches!(
            fault(me, 0o755, stranger, 0o755),
            Some(ObjectServerError::BinaryNotOwned)
        ));
        assert!(matches!(
            fault(me, 0o644, me, 0o755),
            Some(ObjectServerError::BinaryNotExecutable)
        ));
    }

    // C6
    #[test]
    fn a_binary_that_is_missing_or_a_directory_or_relative_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            check_binary(&dir.path().join("missing")),
            Err(ObjectServerError::BinaryIo {
                kind: io::ErrorKind::NotFound,
                ..
            })
        ));
        assert!(matches!(
            check_binary(dir.path()),
            Err(ObjectServerError::BinaryNotAFile)
        ));
        assert!(matches!(
            check_binary(Path::new("rustfs")),
            Err(ObjectServerError::Config(ConfigError::ObjectServerPath))
        ));
    }

    // C7: a symbolic link is followed and the file it names judged and returned.
    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_to_the_binary_is_followed_and_the_target_judged() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let real = dir.path().join("real");
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let resolved = check_binary(&link).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&real).unwrap());
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(matches!(
            check_binary(&link),
            Err(ObjectServerError::BinaryWritable)
        ));
    }
}

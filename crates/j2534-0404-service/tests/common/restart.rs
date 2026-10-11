//! A kill-and-restart harness for restart end-to-end tests (design 5.3, 5.6, 8.2.5; ADR-229).
//!
//! A test file includes it with `#[path = "common/restart.rs"] mod restart;`, builds the job it
//! runs as a [`JobSpec`], and has an entry test that calls [`child_main`]. The harness then
//! gives the test these steps:
//!
//! 1. [`Env::spawn_agent`] starts the agent as a child process (the test executable again, run
//!    on the entry test with [`ROLE_ENV`] set): it launches a real `j2534-0404-service` worker
//!    against the `sim-vci` cdylib and runs the job with the write-job journal. The simulated ECU
//!    keeps its state in a state file (`NGR_SIM_ECU_STATE`, ADR-241), so it outlives the killed
//!    processes, and its timers go on running while nothing has it loaded.
//! 2. [`Env::wait_for_block`] returns once the journal records the given block. It polls the
//!    journal, which is committed and synced before the next instruction runs, so it needs no
//!    cooperation from the child beyond the job itself. The test program parks the child with a
//!    `Wait` instruction right after that block, which leaves the child alive at a known
//!    instruction until the kill arrives.
//! 3. [`AgentChild::kill`] sends SIGKILL to the child's process group, which holds the agent and
//!    its worker: nothing is closed or flushed, as when the host loses everything at once.
//! 4. [`Env::probe`] reads the simulated ECU's session, flash state and power-cycle count from a
//!    separate process (the library keeps the ECU of the first use for the life of its process,
//!    so a probe in the test process would not see later changes), and [`Env::restart`] resumes
//!    the job at library level in this process, with the journal directory, the guards' lock
//!    directory and the ECU state file of the first run.
//! 5. Every process but the probe appends the requests it sends to the ECU, and its battery
//!    voltage reads, to the scenario's request trace (`NGR_SIM_VCI_TRACE`, the trace file of
//!    `sim-vci`). [`Env::restart`] empties the trace before it starts, so [`Env::trace`] returns
//!    the restart's own requests afterwards.
//!
//! A worker crash takes another path: [`Env::crash_worker`] runs the first run in this process,
//! as the agent that survives it, and kills only the worker with SIGKILL once the journal
//! records the given block. The run ends with the lost worker's error and gives its guards back;
//! the harness waits for the killed worker to be reaped and clears the guards' unconfirmed-link
//! mark (ADR-258), as the agent does, and [`Env::restart_on`] resumes on those guards.
//!
//! The harness is for the unix test hosts, where a process group can be killed.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::inputs::ServiceSources;
use agent::journal::{JobKey, Journal, JournalState, Vin};
use agent::{
    JobError, JobLimits, JournalSetup, LinkConfig, resume_program_journaled, run_program_journaled,
};
use diag_ir::{Program, VmState};
use j2534_0404::{FLOW_CONTROL_FILTER, ISO15765, J2534Api0404, PassThruMessage};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// Set in a child process to the role it plays (`agent` or `probe`).
const ROLE_ENV: &str = "NGR_TEST_RESTART_ROLE";
/// What the probe prints before its findings.
const PROBE_PREFIX: &str = "ecu-probe";
/// A wedged service must fail the test, not hang the CI job.
const RESTART_TIMEOUT: Duration = Duration::from_secs(120);

const REQUEST_ID: u32 = 0x7E0;
const RESPONSE_ID: u32 = 0x7E8;

/// The job a restart test runs. The test file builds it in a function the child processes call
/// as well, so the first run, the restart and the probes all see the same program.
pub struct JobSpec {
    pub program: Program,
    /// The ECU's identity fields, as the program's identity sources refer to them.
    pub sources: ServiceSources,
    pub key: JobKey,
    /// The VIN the job targets (well-formed, ADR-261).
    pub vin: String,
}

impl JobSpec {
    fn journal_setup(&self, dir: &Path) -> JournalSetup {
        JournalSetup {
            dir: dir.to_owned(),
            key: self.key.clone(),
            sources: self.sources.clone(),
            vin: Some(Vin::new(self.vin.clone())),
            intended_software_version: None,
        }
    }
}

/// `sim-vci` is a dev-dependency, so cargo builds its cdylib into the same `deps` directory
/// as this test executable.
fn sim_vci_path() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let deps = exe.parent().expect("test executable has a directory");
    let name = format!(
        "{}sim_vci{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let path = deps.join(&name);
    assert!(path.is_file(), "{} should be built", path.display());
    path
}

/// Width of J2534 `unsigned long` in `sim-vci` on this platform.
fn long_size() -> u8 {
    std::mem::size_of::<std::os::raw::c_ulong>() as u8
}

fn launch_options() -> LaunchOptions {
    LaunchOptions {
        // Generous timeouts for a debug binary on a busy CI runner.
        startup_timeout: Duration::from_secs(20),
        request_timeout: Duration::from_secs(5),
        long_size: Some(long_size()),
        ..LaunchOptions::default()
    }
}

/// The directories and files of one restart scenario, removed when it is dropped.
pub struct Env {
    root: PathBuf,
    /// The name of the entry test the child processes run.
    entry: &'static str,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Env {
    /// Creates the scenario's directory with the `sim-vci` configuration and the simulated ECU's
    /// configuration (`ecu_config`, the JSON of `sim_ecu::EcuConfig`). `entry` is the libtest
    /// name of the test file's entry test, which calls [`child_main`].
    pub fn new(tag: &str, entry: &'static str, ecu_config: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ngr-restart-{tag}-{nanos}"));
        std::fs::create_dir_all(root.join("journal")).expect("journal directory");
        let env = Self { root, entry };
        std::fs::write(
            env.config(),
            format!(
                "[config.apis.j2534-0404.libs.{LIBRARY_NAME:?}]\nlibrary_path = {:?}\n",
                sim_vci_path().display().to_string()
            ),
        )
        .expect("test config file should be writable");
        std::fs::write(env.ecu_config(), ecu_config).expect("ECU config file should be writable");
        env
    }

    fn config(&self) -> PathBuf {
        self.root.join("sim-vci.toml")
    }

    fn ecu_config(&self) -> PathBuf {
        self.root.join("ecu.json")
    }

    fn ecu_state(&self) -> PathBuf {
        self.root.join("ecu.state")
    }

    fn trace_file(&self) -> PathBuf {
        self.root.join("requests.trace")
    }

    pub fn journal_dir(&self) -> PathBuf {
        self.root.join("journal")
    }

    fn guard_setup(&self) -> GuardSetup {
        GuardSetup {
            dir: self.root.join("locks"),
            vci: "restart-e2e-vci".to_owned(),
        }
    }

    /// The environment the worker and the library read, for a command that starts a process.
    fn apply_to(&self, command: &mut Command) {
        command
            .env("VCI_CONFIG_PATH", self.config())
            .env("NGR_SIM_ECU_CONFIG", self.ecu_config())
            .env("NGR_SIM_ECU_STATE", self.ecu_state())
            .env("NGR_SIM_VCI_TRACE", self.trace_file())
            .env("NGR_J2534_LONG_SIZE", long_size().to_string())
            .env_remove("VCI_SERVICE_INSECURE_NO_AUTH")
            .env_remove("NGR_SIM_VCI_CONTROL_DIR");
    }

    /// The same environment for this process, whose workers inherit it.
    ///
    /// # Safety
    ///
    /// The caller must make sure no other thread reads or writes the environment: the test
    /// file runs its tests serially and calls this before it builds a runtime.
    unsafe fn apply_here(&self) {
        // SAFETY: the caller's contract.
        unsafe {
            std::env::set_var("VCI_CONFIG_PATH", self.config());
            std::env::set_var("NGR_SIM_ECU_CONFIG", self.ecu_config());
            std::env::set_var("NGR_SIM_ECU_STATE", self.ecu_state());
            std::env::set_var("NGR_SIM_VCI_TRACE", self.trace_file());
            std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
            std::env::remove_var("NGR_SIM_VCI_CONTROL_DIR");
        }
    }

    fn child(&self, role: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test executable path"));
        command
            .args([self.entry, "--exact", "--nocapture"])
            .env(ROLE_ENV, role);
        self.apply_to(&mut command);
        if role == "probe" {
            // The probe's own reads are not part of any scenario's trace.
            command.env_remove("NGR_SIM_VCI_TRACE");
        }
        command
    }

    /// Starts the agent as a child process in a process group of its own, running the job's
    /// first run. It runs until the job ends, which a test normally prevents by killing it.
    pub fn spawn_agent(&self) -> AgentChild {
        let child = self
            .child("agent")
            .process_group(0)
            .stdout(Stdio::null())
            .spawn()
            .expect("the agent process should start");
        AgentChild { child }
    }

    /// Waits until the journal records block `block` (counted from 1, as the host's running
    /// index) and returns what it holds then. Polls the journal without taking its lock, so it
    /// does not disturb the writer; a journal the child has not created yet is waited for.
    pub fn wait_for_block(&self, spec: &JobSpec, block: u32, timeout: Duration) -> JournalState {
        let deadline = Instant::now() + timeout;
        loop {
            let last = Journal::read(&self.journal_dir(), &spec.key);
            if let Ok(state) = &last {
                let reached = state
                    .facts
                    .transfer
                    .as_ref()
                    .and_then(|transfer| transfer.last_block);
                if reached.is_some_and(|reached| reached >= block) {
                    return last.expect("matched Ok");
                }
            }
            assert!(
                Instant::now() < deadline,
                "block {block} was not journaled in {timeout:?}; the journal reads {:?}",
                last.map(|state| state.facts)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Reads the simulated ECU's session and flash state in a process of its own.
    pub fn probe(&self) -> EcuProbe {
        let output = self
            .child("probe")
            .stderr(Stdio::inherit())
            .output()
            .expect("the probe process should start");
        assert!(
            output.status.success(),
            "the probe failed: {}",
            output.status
        );
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text
            .lines()
            .find_map(|line| line.strip_prefix(PROBE_PREFIX))
            .unwrap_or_else(|| panic!("the probe printed no findings: {text}"));
        let field = |name: &str| -> u32 {
            line.split_whitespace()
                .find_map(|part| part.strip_prefix(name))
                .and_then(|value| value.strip_prefix('='))
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("no {name} in {line:?}"))
        };
        EcuProbe {
            session: field("session") as u8,
            flash_phase: field("phase") as u8,
            flash_block: field("block"),
            received: field("received"),
            power_cycles: field("cycles"),
        }
    }

    /// The requests the scenario's processes sent since the last [`Env::restart`] began, or all
    /// of them before it: one line each, as `sim-vci` writes them (`uds 22 F190`, `vbatt`; see
    /// `crates/sim-vci/docs/simulated-vci.md`). Call it when no process of the scenario runs.
    pub fn trace(&self) -> Vec<String> {
        std::fs::read_to_string(self.trace_file())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Resumes the job in this process at library level (`resume_program_journaled`): a new
    /// worker, new guards from the scenario's lock directory, and the journal directory and ECU
    /// state of the first run. Returns the job's result. Must not run while a child of this
    /// scenario is alive. The request trace is emptied first, so [`Env::trace`] afterwards holds
    /// the restart's requests only.
    pub fn restart(&self, spec: &JobSpec) -> Result<VmState, JobError> {
        self.resume_in_process(spec, None)
    }

    /// As [`Env::restart`], on the guards a first run that survived its worker's crash gave
    /// back ([`WorkerCrash::guards`]): the agent passes them on to its next run instead of
    /// taking them again (`resume_program_journaled`'s documentation).
    pub fn restart_on(&self, spec: &JobSpec, guards: JobGuards) -> Result<VmState, JobError> {
        self.resume_in_process(spec, Some(guards))
    }

    fn resume_in_process(
        &self,
        spec: &JobSpec,
        guards: Option<JobGuards>,
    ) -> Result<VmState, JobError> {
        std::fs::write(self.trace_file(), "").expect("the trace file should be writable");
        let runtime = self.runtime();
        runtime
            .block_on(async {
                tokio::time::timeout(RESTART_TIMEOUT, self.resume(spec, guards)).await
            })
            .expect("the restart should finish in time")
    }

    /// Sets the scenario's environment in this process and builds the runtime its runs use.
    fn runtime(&self) -> tokio::runtime::Runtime {
        // SAFETY: every test of the test file that reads or writes the environment is
        // `#[serial]` (the entry test as well, which reads the role variable), so none runs
        // beside this call, and no runtime exists yet; the only other threads are the test
        // harness's own, which do not touch the environment after they started.
        unsafe { self.apply_here() };
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    fn take_guards(&self) -> JobGuards {
        JobGuards::take(
            &self.guard_setup(),
            Duration::from_millis(10),
            &AtomicBool::new(false),
        )
        .expect("the guards should be free")
    }

    async fn resume(&self, spec: &JobSpec, guards: Option<JobGuards>) -> Result<VmState, JobError> {
        let worker = launch_worker().await;
        let client = worker
            .connect(&ConnectOptions::default())
            .await
            .expect("client should connect");
        let guards = guards.unwrap_or_else(|| self.take_guards());
        let (result, guards) = resume_program_journaled(
            client,
            &LinkConfig::iso15765(REQUEST_ID, RESPONSE_ID),
            spec.program.clone(),
            JobLimits::default(),
            spec.journal_setup(&self.journal_dir()),
            guards,
        )
        .await;
        drop(guards);
        worker
            .stop(Duration::from_secs(5))
            .await
            .expect("worker should stop");
        result
    }

    /// Runs the job's first run in this process, the agent's, and kills its worker alone with
    /// SIGKILL once the journal records block `block`: the agent survives and its run ends with
    /// the error the lost worker gives it. Must not run while a child of this scenario is alive.
    pub fn crash_worker(&self, spec: &JobSpec, block: u32, timeout: Duration) -> WorkerCrash {
        let runtime = self.runtime();
        runtime.block_on(async {
            let worker = launch_worker().await;
            let pid = worker.id().expect("the worker runs");
            let client = worker
                .connect(&ConnectOptions::default())
                .await
                .expect("client should connect");
            let guards = self.take_guards();
            let program = spec.program.clone();
            let journal = spec.journal_setup(&self.journal_dir());
            let run = tokio::spawn(async move {
                run_program_journaled(
                    client,
                    &LinkConfig::iso15765(REQUEST_ID, RESPONSE_ID),
                    program,
                    JobLimits::default(),
                    journal,
                    guards,
                )
                .await
            });
            let journal = tokio::task::block_in_place(|| self.wait_for_block(spec, block, timeout));
            let pid = i32::try_from(pid).expect("a process ID fits in pid_t");
            // SAFETY: kill(2) takes a process ID and a signal number and touches no memory.
            let killed = unsafe { libc::kill(pid, libc::SIGKILL) };
            assert_eq!(
                killed,
                0,
                "kill of worker {pid} failed: {}",
                std::io::Error::last_os_error()
            );
            let (result, mut guards) = tokio::time::timeout(RESTART_TIMEOUT, run)
                .await
                .expect("the first run should end once its worker is gone")
                .expect("the first run should not panic");
            // The link could not be closed on a dead worker, so the run marked its guards
            // unconfirmed (ADR-258). Once the worker process is reaped it holds no link, and the
            // agent clears the mark before its next run takes the guards.
            let link_unconfirmed = guards.link_unconfirmed();
            worker
                .stop(Duration::from_secs(5))
                .await
                .expect("the killed worker should be reaped");
            guards.worker_gone();
            WorkerCrash {
                result,
                guards,
                link_unconfirmed,
                journal,
            }
        })
    }
}

/// How a first run whose worker was killed ended ([`Env::crash_worker`]).
pub struct WorkerCrash {
    /// The first run's result.
    pub result: Result<VmState, JobError>,
    /// The guards the run gave back, still held, for the agent's next run. Their
    /// unconfirmed-link mark is cleared once the killed worker was reaped.
    pub guards: JobGuards,
    /// Whether the run had marked the guards' link unconfirmed (ADR-258) before that.
    pub link_unconfirmed: bool,
    /// The journal as it read when the kill was sent.
    pub journal: JournalState,
}

async fn launch_worker() -> WorkerProcess {
    WorkerProcess::launch(
        Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        ServiceKind::J2534V0404,
        LIBRARY_NAME,
        &launch_options(),
    )
    .await
    .expect("worker should launch")
}

/// The agent process of a scenario's first run.
pub struct AgentChild {
    child: Child,
}

impl AgentChild {
    /// Kills the agent and its worker with SIGKILL, waits for the agent and returns how it
    /// ended: the test checks it died of the signal, so a job that finished by itself before
    /// the kill fails the test instead of passing it unnoticed.
    pub fn kill(mut self) -> ExitStatus {
        // The child leads its own process group (`process_group(0)`), which holds its worker.
        let group = i32::try_from(self.child.id()).expect("a process ID fits in pid_t");
        // SAFETY: kill(2) takes a process group and a signal number and touches no memory.
        let result = unsafe { libc::kill(-group, libc::SIGKILL) };
        assert_eq!(
            result,
            0,
            "kill of process group {group} failed: {}",
            std::io::Error::last_os_error()
        );
        self.child
            .wait()
            .expect("the killed agent should be reaped")
    }
}

impl Drop for AgentChild {
    fn drop(&mut self) {
        // A test that failed before the kill must not leave the agent running.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether `status` is the end of a process killed by SIGKILL.
pub fn died_of_sigkill(status: ExitStatus) -> bool {
    status.signal() == Some(9)
}

/// What the simulated ECU reports, from DIDs F186, FD00 and FD01.
#[derive(Debug, PartialEq, Eq)]
pub struct EcuProbe {
    /// The active diagnostic session (1 default, 2 programming).
    pub session: u8,
    /// DID FD00's phase code: 0 idle, 1 erased, 2 transferring, 3 complete, 4 verified,
    /// 5 interrupted.
    pub flash_phase: u8,
    /// DID FD00's block number.
    pub flash_block: u32,
    /// DID FD00's bytes received of the current download.
    pub received: u32,
    /// DID FD01's power-cycle count: ECU resets, power losses and reconnections so far.
    pub power_cycles: u32,
}

/// The entry point of the child processes. A test file calls it from its entry test; it returns
/// at once when the process is not a child of the harness, and otherwise never returns normally
/// for the `agent` role until the job ends (the parent kills it first).
pub fn child_main(spec: fn() -> JobSpec) {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    match role.as_str() {
        "agent" => agent_main(spec()),
        "probe" => probe_main(),
        other => panic!("unknown role {other:?}"),
    }
}

/// The agent's first run, as a process the parent kills.
fn agent_main(spec: JobSpec) {
    let root = PathBuf::from(std::env::var_os("NGR_SIM_ECU_STATE").expect("the state file path"))
        .parent()
        .expect("the state file has a directory")
        .to_path_buf();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let result = runtime.block_on(async {
        let worker = launch_worker().await;
        let client = worker
            .connect(&ConnectOptions::default())
            .await
            .expect("client should connect");
        let guards = JobGuards::take(
            &GuardSetup {
                dir: root.join("locks"),
                vci: "restart-e2e-vci".to_owned(),
            },
            Duration::from_millis(10),
            &AtomicBool::new(false),
        )
        .expect("guards should be free");
        let (result, _guards) = run_program_journaled(
            client,
            &LinkConfig::iso15765(REQUEST_ID, RESPONSE_ID),
            spec.program.clone(),
            JobLimits::default(),
            spec.journal_setup(&root.join("journal")),
            guards,
        )
        .await;
        result
    });
    // Reached only when the job ended before the parent's kill; the parent notices by the exit.
    eprintln!("the agent child's job ended before the kill: {result:?}");
}

fn message(data: &[u8]) -> PassThruMessage {
    PassThruMessage::new(ISO15765, 0, 0, 0, 0, data).expect("message fits")
}

/// Reads the ECU's session and flash state through the library and prints them.
fn probe_main() {
    let api = J2534Api0404::from_path(sim_vci_path()).expect("sim-vci should load");
    let device = api.open(None).expect("the device should open");
    let channel = api
        .connect(device, ISO15765, 0, 500_000)
        .expect("the channel should connect");
    api.start_message_filter(
        channel,
        FLOW_CONTROL_FILTER,
        &mut message(&[0xFF, 0xFF, 0xFF, 0xFF]),
        &mut message(&RESPONSE_ID.to_be_bytes()),
        Some(&mut message(&REQUEST_ID.to_be_bytes())),
    )
    .expect("the filter should start");
    let read_did = |did: u16| -> Vec<u8> {
        let mut data = REQUEST_ID.to_be_bytes().to_vec();
        data.extend_from_slice(&[0x22]);
        data.extend_from_slice(&did.to_be_bytes());
        api.write_messages(channel, &mut [message(&data)], 1_000)
            .expect("the request should be sent");
        let responses = api
            .read_messages(channel, 1, 2_000)
            .expect("the ECU should answer");
        let data = responses[0].data().expect("response data");
        let [high, low] = did.to_be_bytes();
        assert_eq!(data[4..7], [0x62, high, low], "{data:02X?}");
        data[7..].to_vec()
    };
    let session = read_did(0xF186);
    let flash = read_did(0xFD00);
    let cycles = read_did(0xFD01);
    assert_eq!(
        (session.len(), flash.len(), cycles.len()),
        (1, 9, 4),
        "{session:?} {flash:?} {cycles:?}"
    );
    let be = |bytes: &[u8]| u32::from_be_bytes(bytes.try_into().expect("four bytes"));
    println!(
        "{PROBE_PREFIX} session={} phase={} block={} received={} cycles={}",
        session[0],
        flash[0],
        be(&flash[1..5]),
        be(&flash[5..9]),
        be(&cycles)
    );
}

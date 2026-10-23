//! A kill-and-restart harness for restart end-to-end tests (design 5.3, 5.6, 8.2.5; ADR-229).
//!
//! A test file includes it with `#[path = "common/restart.rs"] mod restart;`, builds the job it
//! runs as a [`JobSpec`] (by variant, when it runs more than one: [`Env::with_variant`]), and
//! has an entry test that calls [`child_main`]. The harness then
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
//! 3. [`AgentChild::kill`] ends the agent and its worker at once: SIGKILL to the child's
//!    process group on unix, `TerminateJobObject` on the job object the child was put in on
//!    Windows (it starts suspended and resumes once it is in the job, so its worker is created
//!    in the job too). Nothing is closed or flushed, as when the host loses everything at once. Both
//!    take effect within the call, so the kill lands inside the program's `Wait` even on a loaded
//!    runner, where starting a `taskkill` process took longer than the wait.
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
//! The workers of the first run and of the restart also take `sim-vci` control commands from
//! the scenario's control directory (`NGR_SIM_VCI_CONTROL_DIR`), which [`Env::inject`] writes,
//! so a test can inject a fault while the program waits, or before the restart; the probe gets
//! no control directory.
//!
//! A worker crash takes another path: [`Env::crash_worker`] runs the first run in this process,
//! as the agent that survives it, and kills only the worker with SIGKILL once the journal
//! records the given block. The run ends with the lost worker's error and gives its guards back;
//! the harness waits for the killed worker to be reaped and clears the guards' unconfirmed-link
//! mark (ADR-258), as the agent does, and [`Env::restart_on`] resumes on those guards.
//! [`Env::fault_mid_run`] runs the first run the same way, but hands the worker a control
//! command at that block instead of killing it (an ECU power loss, a VCI disconnect), and stops
//! the worker once the run has ended.
//!
//! On Windows, `TerminateProcess` stops a process at once as SIGKILL does:
//! [`Env::crash_worker`] ends the worker with it, and [`AgentChild::kill`] ends the agent and
//! its worker together with `TerminateJobObject`, which does the same for every process in the
//! job object [`Env::spawn_agent`] put the agent in.

#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardError, GuardSetup, JobGuards};
use agent::inputs::ServiceSources;
use agent::journal::{JobKey, Journal, JournalState, Vin};
use agent::{
    Artifact, JobDocument, JobError, JobLimits, JobRun, JournalSetup, LinkConfig,
    resume_program_journaled, run_program_journaled,
};
use diag_ir::{Program, VmState};
use j2534_0404::{FLOW_CONTROL_FILTER, ISO15765, J2534Api0404, PassThruMessage};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// Set in a child process to the role it plays (`agent` or `probe`).
const ROLE_ENV: &str = "NGR_TEST_RESTART_ROLE";
/// Set in a child process to the scenario's job variant ([`Env::with_variant`]).
const VARIANT_ENV: &str = "NGR_TEST_RESTART_VARIANT";
/// What the probe prints before its findings.
const PROBE_PREFIX: &str = "ecu-probe";
/// A wedged service must fail the test, not hang the CI job.
const RESTART_TIMEOUT: Duration = Duration::from_secs(120);

const REQUEST_ID: u32 = 0x7E0;
const RESPONSE_ID: u32 = 0x7E8;

/// The job a restart test runs. The test file builds it in a function the child processes call
/// as well, so the first run, the restart and the probes all see the same program.
pub struct JobSpec {
    /// The job's program; the procedure of [`JobSpec::document`] when there is one.
    pub program: Program,
    /// The ECU document the program comes from, for a procedure converted from a definition:
    /// the job is built from it ([`agent::Job::from_document`]), so the VM decodes through its
    /// declarations. `None` for a hand-built program.
    pub document: Option<Vec<u8>>,
    /// The job's flash artifact, which a converted plan's `FlashBlock` reads (ADR-336 item 1).
    pub artifact: Option<Vec<u8>>,
    /// The ECU's identity fields, as the program's identity sources refer to them.
    pub sources: ServiceSources,
    pub key: JobKey,
    /// The VIN the job targets (well-formed, ADR-261).
    pub vin: String,
    /// The software version the job writes, as the ECU reports it (`None` when the job names
    /// none): with one, a restart after the completed plan takes the read-back path (ADR-271).
    pub intended_software_version: Option<Vec<u8>>,
}

impl JobSpec {
    /// The job the agent runs: from the document when there is one, with the artifact. Named by
    /// its path, since on Windows this file's `Job` is the job object the agent child runs in.
    fn job(&self) -> agent::Job {
        let job = match &self.document {
            Some(document) => agent::Job::from_document(
                JobDocument::new(document.clone()).expect("the document loads"),
            )
            .expect("the document carries its procedure"),
            None => agent::Job::from(self.program.clone()),
        };
        match &self.artifact {
            Some(bytes) => {
                let artifact = Artifact::from_bytes(bytes.clone());
                job.with_artifact(*artifact.sha256(), artifact)
            }
            None => job,
        }
    }

    /// The link the job runs on: the one its document declares, or the harness's ISO 15765 link
    /// for a hand-built program.
    fn link(&self) -> LinkConfig {
        match &self.document {
            Some(_) => self
                .job()
                .declared_link()
                .expect("the document declares its link"),
            None => LinkConfig::iso15765(REQUEST_ID, RESPONSE_ID),
        }
    }

    fn journal_setup(&self, dir: &Path) -> JournalSetup {
        JournalSetup {
            dir: dir.to_owned(),
            key: self.key.clone(),
            sources: self.sources.clone(),
            vin: Some(Vin::new(self.vin.clone())),
            intended_software_version: self.intended_software_version.clone(),
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
    /// The job variant the agent child builds ([`Env::with_variant`]).
    variant: &'static str,
    /// How many control commands [`Env::inject`] has written.
    commands: std::cell::Cell<u32>,
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
        Self::with_variant(tag, entry, "", ecu_config)
    }

    /// As [`Env::new`], for a test file whose entry builds more than one job: the agent child
    /// gets `variant`, which [`child_main`] hands to the file's job builder, so it runs the
    /// same job as the test that builds it with that variant. [`Env::new`] uses `""`.
    pub fn with_variant(
        tag: &str,
        entry: &'static str,
        variant: &'static str,
        ecu_config: &str,
    ) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ngr-restart-{tag}-{nanos}"));
        std::fs::create_dir_all(root.join("journal")).expect("journal directory");
        std::fs::create_dir_all(root.join("control")).expect("control directory");
        let env = Self {
            root,
            entry,
            variant,
            commands: std::cell::Cell::new(0),
        };
        std::fs::write(env.config(), "").expect("test config file should be writable");
        // The registration definition names the library, through the debug-only
        // `NGR_J2534_DEFINITION_DIR` override (ADR-282).
        std::fs::create_dir_all(env.definitions()).expect("definition directory");
        j2534_0404_mock::write_definition(&env.definitions(), LIBRARY_NAME, &sim_vci_path())
            .expect("definition file should be writable");
        std::fs::write(env.ecu_config(), ecu_config).expect("ECU config file should be writable");
        env
    }

    fn config(&self) -> PathBuf {
        self.root.join("sim-vci.toml")
    }

    fn definitions(&self) -> PathBuf {
        self.root.join("definitions")
    }

    fn ecu_config(&self) -> PathBuf {
        self.root.join("ecu.json")
    }

    fn ecu_state(&self) -> PathBuf {
        self.root.join("ecu.state")
    }

    fn control_dir(&self) -> PathBuf {
        self.root.join("control")
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
            .env("NGR_J2534_DEFINITION_DIR", self.definitions())
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
            std::env::set_var("NGR_J2534_DEFINITION_DIR", self.definitions());
            std::env::set_var("NGR_SIM_ECU_CONFIG", self.ecu_config());
            std::env::set_var("NGR_SIM_ECU_STATE", self.ecu_state());
            std::env::set_var("NGR_SIM_VCI_TRACE", self.trace_file());
            std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
            std::env::set_var("NGR_SIM_VCI_CONTROL_DIR", self.control_dir());
        }
    }

    fn child(&self, role: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test executable path"));
        command
            .args([self.entry, "--exact", "--nocapture"])
            .env(ROLE_ENV, role)
            .env(VARIANT_ENV, self.variant);
        self.apply_to(&mut command);
        match role {
            // The probe's own reads are not part of any scenario's trace.
            "probe" => {
                command.env_remove("NGR_SIM_VCI_TRACE");
            }
            // The first run's worker takes control commands ([`Env::inject`]).
            "agent" => {
                command.env("NGR_SIM_VCI_CONTROL_DIR", self.control_dir());
            }
            _ => {}
        }
        command
    }

    /// Starts the agent as a child process running the job's first run, on unix in a process
    /// group of its own, on Windows in a job object of its own. It runs until the job ends, which
    /// a test normally prevents by killing it.
    pub fn spawn_agent(&self) -> AgentChild {
        let mut command = self.child("agent");
        #[cfg(unix)]
        command.process_group(0);
        // Suspended until it is in its job, so that the worker it starts is created in the job
        // too, however late the assignment runs on a loaded runner.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(
            &mut command,
            windows_sys::Win32::System::Threading::CREATE_SUSPENDED,
        );
        let child = command
            .stdout(Stdio::null())
            .spawn()
            .expect("the agent process should start");
        #[cfg(windows)]
        let (child, job) = start_in_job(child);
        AgentChild {
            child: Some(child),
            #[cfg(windows)]
            job,
        }
    }

    /// Waits until the journal records block `block` (counted from 1, as the host's running
    /// index) and returns what it holds then, as [`Env::wait_for`].
    pub fn wait_for_block(&self, spec: &JobSpec, block: u32, timeout: Duration) -> JournalState {
        self.wait_for(spec, &format!("block {block}"), timeout, |state| {
            state
                .facts
                .transfer
                .as_ref()
                .and_then(|transfer| transfer.last_block)
                .is_some_and(|reached| reached >= block)
        })
    }

    /// Waits until `reached` holds for the journal and returns what the journal held then.
    /// Polls the journal without taking its lock, so it does not disturb the writer; a journal
    /// the child has not created yet is waited for. `what` names the condition in the failure.
    pub fn wait_for(
        &self,
        spec: &JobSpec,
        what: &str,
        timeout: Duration,
        reached: impl Fn(&JournalState) -> bool,
    ) -> JournalState {
        let deadline = Instant::now() + timeout;
        loop {
            let last = Journal::read(&self.journal_dir(), &spec.key);
            if let Ok(state) = &last
                && reached(state)
            {
                return last.expect("matched Ok");
            }
            assert!(
                Instant::now() < deadline,
                "{what} was not reached in {timeout:?}; the journal reads {:?}",
                last.map(|state| state.facts)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Hands `command` (a `sim-vci` control command, JSON) to the scenario's worker through
    /// the control directory: written under a temporary name and renamed, so `sim-vci` never
    /// reads it half-written, and numbered, so commands apply in the order they were given.
    /// `sim-vci` applies it at the worker's next J2534 call: the first run's, or, written while no
    /// process of the scenario runs, the restart's first.
    pub fn inject(&self, command: &str) {
        let number = self.commands.get() + 1;
        self.commands.set(number);
        let temporary = self.control_dir().join(format!("{number:03}.tmp"));
        std::fs::write(&temporary, command).expect("the control file should be writable");
        std::fs::rename(
            &temporary,
            self.control_dir().join(format!("{number:03}.json")),
        )
        .expect("the control file should be renamed");
    }

    /// Whether the control directory is empty: `sim-vci` applied and deleted every command, and
    /// rejected none.
    pub fn commands_applied(&self) -> bool {
        std::fs::read_dir(self.control_dir())
            .expect("control directory")
            .next()
            .is_none()
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
        self.resume_in_process(spec, None).result
    }

    /// As [`Env::restart`], on the guards an interrupted first run gave
    /// back ([`InterruptedRun::guards`]): the agent passes them on to its next run instead of
    /// taking them again (`resume_program_journaled`'s documentation).
    pub fn restart_on(&self, spec: &JobSpec, guards: JobGuards) -> Result<VmState, JobError> {
        self.resume_in_process(spec, Some(guards)).result
    }

    /// As [`Env::restart_on`], but the guards the run gives back are kept for a further run, as
    /// [`Env::fault_mid_run`] keeps them: the worker is stopped, and the guards' unconfirmed-link
    /// mark (ADR-258) is cleared once it has exited.
    pub fn restart_keeping(&self, spec: &JobSpec, guards: JobGuards) -> InterruptedRun {
        self.resume_in_process(spec, Some(guards))
    }

    fn resume_in_process(&self, spec: &JobSpec, guards: Option<JobGuards>) -> InterruptedRun {
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

    async fn resume(&self, spec: &JobSpec, guards: Option<JobGuards>) -> InterruptedRun {
        let worker = launch_worker().await;
        let client = worker
            .connect(&ConnectOptions::default())
            .await
            .expect("client should connect");
        let guards = guards.unwrap_or_else(|| self.take_guards());
        let JobRun { result, guards, .. } = resume_program_journaled(
            client,
            &spec.link(),
            spec.job(),
            JobLimits::default(),
            spec.journal_setup(&self.journal_dir()),
            guards,
        )
        .await;
        let link_unconfirmed = guards.link_unconfirmed();
        worker
            .stop(Duration::from_secs(5))
            .await
            .expect("worker should stop");
        let mut guards = guards;
        guards.worker_gone();
        let journal = Journal::read(&self.journal_dir(), &spec.key).expect("journal");
        InterruptedRun {
            result,
            guards,
            link_unconfirmed,
            journal,
        }
    }

    /// Runs the job's first run in this process, the agent's, and kills its worker alone with
    /// SIGKILL once the journal records block `block`: the agent survives and its run ends with
    /// the error the lost worker gives it. Must not run while a child of this scenario is alive.
    pub fn crash_worker(&self, spec: &JobSpec, block: u32, timeout: Duration) -> InterruptedRun {
        self.interrupt_run(spec, block, timeout, Interruption::KillWorker)
    }

    /// Runs the job's first run in this process, the agent's, and hands its worker `command`
    /// through the control directory once the journal records block `block`: `sim-vci` applies
    /// it at the worker's next J2534 call (its receive poll, while the program waits), and the
    /// agent's run ends with the error its next request meets. The worker is stopped after the
    /// run ended. Must not run while a child of this
    /// scenario is alive.
    pub fn fault_mid_run(
        &self,
        spec: &JobSpec,
        block: u32,
        timeout: Duration,
        command: &str,
    ) -> InterruptedRun {
        self.interrupt_run(spec, block, timeout, Interruption::Inject(command))
    }

    fn interrupt_run(
        &self,
        spec: &JobSpec,
        block: u32,
        timeout: Duration,
        interruption: Interruption<'_>,
    ) -> InterruptedRun {
        let runtime = self.runtime();
        runtime.block_on(async {
            let worker = launch_worker().await;
            let pid = worker.id().expect("the worker runs");
            let client = worker
                .connect(&ConnectOptions::default())
                .await
                .expect("client should connect");
            let guards = self.take_guards();
            let program = spec.job();
            let link = spec.link();
            let journal = spec.journal_setup(&self.journal_dir());
            let run = tokio::spawn(async move {
                run_program_journaled(
                    client,
                    &link,
                    program,
                    JobLimits::default(),
                    journal,
                    guards,
                )
                .await
            });
            let journal = tokio::task::block_in_place(|| self.wait_for_block(spec, block, timeout));
            match interruption {
                Interruption::KillWorker => kill_worker(pid),
                Interruption::Inject(command) => self.inject(command),
            }
            let mut run = run;
            let JobRun {
                result, mut guards, ..
            } = match tokio::time::timeout(RESTART_TIMEOUT, &mut run).await {
                Ok(joined) => joined.expect("the first run should not panic"),
                Err(_) => {
                    // Dropping the run cancels the job, so the runtime's drop does not wait for
                    // it for ever.
                    run.abort();
                    panic!("the first run should end once it is interrupted");
                }
            };
            // A link that could not be closed (on a dead worker, or on a lost device) marks the
            // guards unconfirmed (ADR-258). Once the worker process has exited it holds no link,
            // and the agent clears the mark before its next run takes the guards.
            let link_unconfirmed = guards.link_unconfirmed();
            worker
                .stop(Duration::from_secs(5))
                .await
                .expect("the worker should stop or be reaped");
            guards.worker_gone();
            InterruptedRun {
                result,
                guards,
                link_unconfirmed,
                journal,
            }
        })
    }

    /// Whether the scenario's guards are held by someone else: a `JobGuards::take` on its lock
    /// directory is still waiting when it is cancelled a while later. The cancel is set late,
    /// since the take checks it before its first attempt.
    pub fn guards_are_held(&self) -> bool {
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let canceller = {
            let cancelled = cancelled.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        };
        let taken = JobGuards::take(&self.guard_setup(), Duration::from_millis(10), &cancelled);
        canceller.join().expect("the canceller should not panic");
        match taken {
            Err(GuardError::Cancelled) => true,
            Ok(_) => false,
            Err(other) => panic!("taking the guards failed: {other}"),
        }
    }
}

/// How the first run is interrupted ([`Env::interrupt_run`]).
enum Interruption<'a> {
    /// The worker alone is killed with SIGKILL.
    KillWorker,
    /// The worker is handed this control command.
    Inject(&'a str),
}

/// How a first run that was interrupted ended ([`Env::crash_worker`], [`Env::fault_mid_run`]).
pub struct InterruptedRun {
    /// The first run's result.
    pub result: Result<VmState, JobError>,
    /// The guards the run gave back, still held, for the agent's next run. Their
    /// unconfirmed-link mark is cleared once the worker was stopped or reaped.
    pub guards: JobGuards,
    /// Whether the run had marked the guards' link unconfirmed (ADR-258) before that.
    pub link_unconfirmed: bool,
    /// The journal as it read just before the interruption, or after the run for
    /// [`Env::restart_keeping`].
    pub journal: JournalState,
}

async fn launch_worker() -> WorkerProcess {
    WorkerProcess::launch(
        Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        ServiceKind::J2534V0404,
        LIBRARY_NAME,
        OperatingMode::User,
        &launch_options(),
    )
    .await
    .expect("worker should launch")
}

/// Ends the worker process `pid` at once, as a crash would: nothing in it is closed or flushed.
#[cfg(unix)]
fn kill_worker(pid: u32) {
    let pid = i32::try_from(pid).expect("a process ID fits in pid_t");
    // SAFETY: kill(2) takes a process ID and a signal number and touches no memory.
    let killed = unsafe { libc::kill(pid, libc::SIGKILL) };
    assert_eq!(
        killed,
        0,
        "kill of worker {pid} failed: {}",
        std::io::Error::last_os_error()
    );
}

/// Ends the worker process `pid` at once, as a crash would: `TerminateProcess`, so nothing in
/// it is closed or flushed.
#[cfg(windows)]
fn kill_worker(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    // SAFETY: OpenProcess takes an access mask, a flag and a process ID and touches no memory.
    let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    assert!(
        !process.is_null(),
        "opening worker {pid} failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: `process` is the handle opened above, closed only after this call.
    let terminated = unsafe { TerminateProcess(process, 1) };
    let error = std::io::Error::last_os_error();
    // SAFETY: `process` is open and not used after this.
    unsafe { CloseHandle(process) };
    assert_ne!(terminated, 0, "terminating worker {pid} failed: {error}");
}

/// Ends the agent process `pid` and its worker at once. The agent leads its own process group
/// (`process_group(0)`), which holds its worker, and SIGKILL to the group ends both.
#[cfg(unix)]
fn kill_agent_and_worker(pid: u32) -> std::io::Result<()> {
    let group = i32::try_from(pid).expect("a process ID fits in pid_t");
    // SAFETY: kill(2) takes a process group and a signal number and touches no memory.
    if unsafe { libc::kill(-group, libc::SIGKILL) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The job object an agent of a scenario runs in, with every process it starts, so the agent
/// and its worker end together in one call.
#[cfg(windows)]
struct Job(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Job {
    /// A new job object holding `child`. Closing the job's handle, which only the test holds,
    /// terminates whatever it still holds, so a test that fails on any path, a failed kill
    /// included, leaves neither the agent nor its worker running.
    fn holding(child: &Child) -> std::io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // SAFETY: no security attributes and no name; the call touches no memory of ours.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Job(handle);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the handle is open, and `limits` is the structure the class names, of the size
        // passed.
        let limited = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if limited == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both handles are open: the job's above, the child's while `child` lives.
        if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Terminates every process in the job with exit code 1, as `taskkill /F` would, and waits
    /// until none is left: a process whose termination waits on pending I/O can outlive the call,
    /// and the next step of a scenario must not find the old worker still using the VCI.
    fn terminate(&self) -> std::io::Result<()> {
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
            QueryInformationJobObject, TerminateJobObject,
        };
        // SAFETY: the job's handle is open until the drop.
        if unsafe { TerminateJobObject(self.0, 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // SAFETY: the handle is open, and `info` is the structure the class names, of the
            // size passed.
            let queried = unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    (&raw mut info).cast(),
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                )
            };
            if queried == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if info.ActiveProcesses == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::other(format!(
                    "{} processes of the job still run 30 s after its termination",
                    info.ActiveProcesses
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Puts `child`, created suspended, in a job object of its own and resumes it. On a failure the
/// child is killed before the panic, since dropping a [`Child`] leaves it running (here, suspended
/// for good).
#[cfg(windows)]
fn start_in_job(mut child: Child) -> (Child, Job) {
    match Job::holding(&child).and_then(|job| resume(&child).map(|()| job)) {
        Ok(job) => (child, job),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("starting the agent in its job object failed: {error}");
        }
    }
}

/// Resumes the only thread of `child`, which was created suspended.
#[cfg(windows)]
fn resume(child: &Child) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    // SAFETY: the call touches no memory of ours.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut thread_id = None;
    // SAFETY: the snapshot is open and `entry` is a THREADENTRY32 with its size set.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == child.id() {
            thread_id = Some(entry.th32ThreadID);
            break;
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: the snapshot is open and not used after this.
    unsafe { CloseHandle(snapshot) };
    let thread_id =
        thread_id.ok_or_else(|| std::io::Error::other("the suspended agent has no thread"))?;
    // SAFETY: the call touches no memory of ours.
    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    if thread.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the thread's handle is open; it is closed right after.
    let resumed = unsafe { ResumeThread(thread) };
    let error = std::io::Error::last_os_error();
    // SAFETY: the handle is open and not used after this.
    unsafe { CloseHandle(thread) };
    if resumed == u32::MAX {
        return Err(error);
    }
    Ok(())
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle is open and not used after this.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// The agent process of a scenario's first run.
pub struct AgentChild {
    /// `None` once [`AgentChild::kill`] has reaped the agent, so the drop does not kill again.
    child: Option<Child>,
    /// The job object holding the agent and its worker.
    #[cfg(windows)]
    job: Job,
}

impl AgentChild {
    /// Ends the agent and its worker at once.
    fn end(&self, child: &Child) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let _ = self;
            kill_agent_and_worker(child.id())
        }
        #[cfg(windows)]
        {
            let _ = child;
            self.job.terminate()
        }
    }
}

impl AgentChild {
    /// Kills the agent and its worker at once, waits for the agent and returns how it ended: the
    /// test checks that it was killed ([`died_of_kill`]), so a job that finished by itself before
    /// the kill fails the test instead of passing it unnoticed.
    pub fn kill(mut self) -> ExitStatus {
        let mut child = self.child.take().expect("the agent is reaped only here");
        let pid = child.id();
        if let Err(error) = self.end(&child) {
            panic!("kill of agent {pid} and its worker failed: {error}");
        }
        let status = child.wait().expect("the killed agent should be reaped");
        // Reaped, and taken: its PID and process group may be reused, so the drop must not
        // signal them.
        status
    }
}

impl Drop for AgentChild {
    fn drop(&mut self) {
        // A test that failed before the kill must leave neither the agent nor its worker
        // running; the agent may have ended by then, so a failure here is not reported. After a
        // failed kill the agent alone is killed before the wait, which could otherwise block for
        // good; on Windows closing the job, after this returns, ends a worker left running.
        if let Some(mut child) = self.child.take() {
            if self.end(&child).is_err() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

/// Whether `status` is the end of a process [`AgentChild::kill`] killed: by SIGKILL on unix, by
/// `TerminateJobObject` with exit code 1 on Windows. A job that ended by
/// itself exits 0, and a panicking test 101.
pub fn died_of_kill(status: ExitStatus) -> bool {
    #[cfg(unix)]
    {
        status.signal() == Some(9)
    }
    #[cfg(windows)]
    {
        status.code() == Some(1)
    }
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

/// The entry point of the child processes. A test file calls it from its entry test with its
/// job builder, which takes the scenario's variant ([`Env::with_variant`]); it returns at once
/// when the process is not a child of the harness, and otherwise never returns normally for the
/// `agent` role until the job ends (the parent kills it first).
pub fn child_main(spec: fn(&str) -> JobSpec) {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let variant = std::env::var(VARIANT_ENV).unwrap_or_default();
    match role.as_str() {
        "agent" => agent_main(spec(&variant)),
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
        let JobRun {
            result,
            guards: _guards,
            ..
        } = run_program_journaled(
            client,
            &spec.link(),
            spec.job(),
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

//! `ngr-agent`: the device-side agent (design 3.3). The job runner lives in the `agent` library.
//! The binary's one command, `ngr-agent run`, runs the procedure of a variant's document on a
//! VCI and prints the final VM state as JSON (`crates/agent/docs/ngr-agent.md`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::artifact::{Artifact, SHA256_LEN, parse_sha256};
use agent::guards::{GuardSetup, JobGuards};
use agent::journal::JobKey;
use agent::launch::{Api, launch_worker};
use agent::policy::build_ceiling;
use agent::{CommLogSetup, Job, JobDocument, JobLimits, JobRun, check_job, run_program};
use diag_ir::{Value, VmState};
use worker_host::service::{LaunchOptions, OperatingMode, Stopped, WorkerLayout};

const USAGE: &str = "\
usage: ngr-agent run --vci <name> --mode <device|user> --document <file> [options]

Runs the procedure of a variant's document on a J2534 v04.04 or D-PDU API VCI and prints the
final VM state as JSON on stdout. The job takes its link from the protocol the variant
declares.

options:
  --vci <name>         library name the worker service resolves: a J2534 library, or the
                       SHORT_NAME of a D-PDU API implementation
  --api <j2534|dpdu>   the API the VCI's library implements (default j2534)
  --mode <device|user> the device's operating mode, passed to the worker: device mode
                       refuses a library that fails the pre-load checks, user mode
                       reports the findings and loads it
  --document <file>    a variant's document (.dir): runs its procedure, which decodes
                       response fields through the variant's declarations (required)
  --artifact <file>    the flash artifact the procedure reads its blocks from, read once
                       before any worker starts; needs --artifact-sha256
  --artifact-sha256 <hex>
                       the SHA-256 the job names for the artifact, 64 hexadecimal digits;
                       the job is refused when the file hashes to something else
  --workers <dir>      worker binaries, laid out as <dir>/<ABI name>/<binary>
                       (default: the 'workers' directory next to ngr-agent)
  --locks <dir>        the device's lock directory for the VCI lock and the reprogramming
                       slot (default: the 'locks' directory next to ngr-agent)
  --comm-log <dir>     writes the run's communication log (design 8.11) to this directory,
                       created if missing, and prints the log's path on stderr; without it
                       the run writes none";

/// How often a job waiting for its guards tries again.
const GUARD_POLL: Duration = Duration::from_millis(10);

/// How long a stopping worker may take to close its link before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
struct RunArgs {
    api: Api,
    vci: String,
    mode: OperatingMode,
    /// The variant's document, whose procedure is the program (design 8.2.6). The job takes
    /// the link its variant declares (ADR-324).
    document: PathBuf,
    /// The flash artifact file and the SHA-256 the job names for it: both or neither
    /// (design 8.2.5, ADR-336 item 1).
    artifact: Option<(PathBuf, [u8; SHA256_LEN])>,
    workers: Option<PathBuf>,
    locks: Option<PathBuf>,
    /// The directory of the run's communication log (design 8.11, ADR-347); none without it.
    comm_log: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("ngr-agent: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    // The job's VM runs on a blocking thread that blocks on the runtime for every primitive,
    // so the runtime must be multi-threaded (ADR-235).
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ngr-agent: cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(args)) {
        Ok(state) => {
            println!("{state}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("ngr-agent: {message}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<RunArgs, String> {
    let mut args = args.into_iter();
    match args.next() {
        Some(command) if command == "run" => {}
        Some(command) => return Err(format!("unknown command {command:?}")),
        None => return Err("missing command".to_owned()),
    }
    let mut api = Api::J2534;
    let mut vci = None;
    let mut mode = None;
    let mut document = None;
    let mut artifact = None;
    let mut artifact_sha256 = None;
    let mut workers = None;
    let mut locks = None;
    let mut comm_log = None;
    while let Some(flag) = args.next() {
        let flag = flag
            .into_string()
            .map_err(|flag| format!("unknown option {flag:?}"))?;
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--vci" => {
                vci = Some(
                    value
                        .into_string()
                        .map_err(|value| format!("--vci {value:?} is not valid UTF-8"))?,
                )
            }
            "--mode" => {
                mode = Some(
                    value
                        .to_str()
                        .ok_or_else(|| format!("--mode {value:?} is not valid UTF-8"))?
                        .parse::<OperatingMode>()
                        .map_err(|error| format!("--mode: {error}"))?,
                )
            }
            "--api" => {
                api = value
                    .to_str()
                    .ok_or_else(|| format!("--api {value:?} is not valid UTF-8"))?
                    .parse()
                    .map_err(|error| format!("--api: {error}"))?
            }
            "--document" if document.is_some() => {
                return Err("give --document once".to_owned());
            }
            "--document" => document = Some(PathBuf::from(value)),
            "--artifact" if artifact.is_some() => {
                return Err("give --artifact once".to_owned());
            }
            "--artifact" => artifact = Some(PathBuf::from(value)),
            "--artifact-sha256" if artifact_sha256.is_some() => {
                return Err("give --artifact-sha256 once".to_owned());
            }
            "--artifact-sha256" => {
                artifact_sha256 = Some(
                    parse_sha256(value.to_str().ok_or_else(|| {
                        format!("--artifact-sha256 {value:?} is not valid UTF-8")
                    })?)
                    .map_err(|error| format!("--artifact-sha256: {error}"))?,
                )
            }
            "--workers" => workers = Some(PathBuf::from(value)),
            // A relative path would name a different lock set from each working directory.
            "--locks" if !Path::new(&value).is_absolute() => {
                return Err("--locks needs an absolute directory".to_owned());
            }
            "--locks" => locks = Some(PathBuf::from(value)),
            "--comm-log" if comm_log.is_some() => {
                return Err("give --comm-log once".to_owned());
            }
            "--comm-log" => comm_log = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown option {flag:?}")),
        }
    }
    let artifact = match (artifact, artifact_sha256) {
        (Some(path), Some(sha256)) => Some((path, sha256)),
        (None, None) => None,
        _ => return Err("--artifact and --artifact-sha256 must be given together".to_owned()),
    };
    Ok(RunArgs {
        api,
        vci: vci.ok_or("--vci is required")?,
        // The mode is fixed per device at installation (design 6.2); there is no default.
        mode: mode.ok_or("--mode is required")?,
        document: document.ok_or("--document is required")?,
        artifact,
        workers,
        locks,
        comm_log,
    })
}

/// The `workers` directory next to the running executable.
fn default_workers() -> Result<PathBuf, String> {
    next_to_exe("workers", "--workers")
}

/// The `locks` directory next to the running executable.
fn default_locks() -> Result<PathBuf, String> {
    next_to_exe("locks", "--locks")
}

fn next_to_exe(name: &str, flag: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot locate ngr-agent: {error}; pass {flag}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| format!("cannot locate the directory of ngr-agent; pass {flag}"))?;
    Ok(dir.join(name))
}

/// Reads the job a run's document gives.
fn read_job(path: &Path) -> Result<Job, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    JobDocument::new(bytes)
        .and_then(Job::from_document)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Reads the artifact file once and gives it to the job under the hash the command line names;
/// [`check_job`] compares the two.
fn with_artifact(job: Job, artifact: Option<(PathBuf, [u8; SHA256_LEN])>) -> Result<Job, String> {
    let Some((path, sha256)) = artifact else {
        return Ok(job);
    };
    let artifact = Artifact::read(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    Ok(job.with_artifact(sha256, artifact))
}

/// The communication log of this run: a job ID minted from the clock and the process ID, so
/// two runs never share a log, in generation 0 (ADR-347). On Unix a missing directory is created
/// writable and readable by its owner only, since the log holds the vehicle's data; an existing
/// one keeps its permissions, and the log file itself is its owner's alone (`agent::CommLog`).
fn comm_log_setup(dir: PathBuf) -> Result<CommLogSetup, String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let setup = CommLogSetup {
        dir,
        key: JobKey {
            job_id: shared_proto::JobId(format!("run-{unix_ms}-{}", std::process::id())),
            generation: 0,
        },
    };
    setup
        .path()
        .map_err(|error| format!("--comm-log: {error}"))?;
    Ok(setup)
}

/// Runs the job and returns the final VM state as JSON.
async fn run(args: RunArgs) -> Result<String, String> {
    let mut job = with_artifact(read_job(&args.document)?, args.artifact)?;
    // A program the job would refuse does not need a worker, nor does a link the document
    // declares that cannot be used.
    check_job(&job, build_ceiling()).map_err(|error| format!("job failed: {error}"))?;
    let link = job
        .declared_link()
        .map_err(|error| format!("job failed: {error}"))?;
    // Only a job that passed its check gets a log, so a refused one creates no directory.
    let mut log_path = None;
    if let Some(dir) = args.comm_log {
        let setup = comm_log_setup(dir)?;
        log_path = setup.path().ok();
        job = job.with_comm_log(setup);
    }
    // The per-VCI lock, and the reprogramming slot for a program that writes (design 8.8.1). The
    // wait polls a file lock, so it runs off the runtime's threads; nothing cancels it here.
    let dir = match args.locks {
        Some(dir) => dir,
        None => default_locks()?,
    };
    let setup = GuardSetup {
        dir,
        vci: args.vci.clone(),
    };
    let writes = job.writes();
    let guards = tokio::task::spawn_blocking(move || {
        let never = AtomicBool::new(false);
        if writes {
            JobGuards::take(&setup, GUARD_POLL, &never)
        } else {
            JobGuards::take_vci_only(&setup, GUARD_POLL, &never)
        }
    })
    .await
    .map_err(|error| format!("taking the job's guards failed: {error}"))?
    .map_err(|error| format!("cannot take the job's guards: {error}"))?;
    let workers = WorkerLayout {
        root: match args.workers {
            Some(root) => root,
            None => default_workers()?,
        },
    };
    let worker = launch_worker(
        args.api,
        &args.vci,
        args.mode,
        &workers,
        &LaunchOptions::default(),
    )
    .await
    .map_err(|error| error.to_string())?;
    let JobRun {
        result,
        mut guards,
        comm_log,
    } = run_program(worker.client, &link, job, JobLimits::default(), guards).await;
    // The path is printed only once the log exists: a job that never ran, or a log that could
    // not be opened, names none.
    match (&comm_log, &log_path) {
        (agent::CommLogStatus::None, _) => {}
        (agent::CommLogStatus::Incomplete(agent::CommLogIncomplete::NotOpened(kind)), _) => {
            eprintln!("ngr-agent: communication log not opened: {kind:?}");
        }
        (status, Some(path)) => {
            eprintln!("ngr-agent: communication log: {}", path.display());
            if let agent::CommLogStatus::Incomplete(incomplete) = status {
                eprintln!("ngr-agent: communication log incomplete: {incomplete:?}");
            }
        }
        (_, None) => {}
    }
    // The job has closed its link whatever the result, unless its guards say otherwise
    // (ADR-258). A stop that reaped the worker means the worker no longer holds the link; a
    // vendor device server may still hold the device a while, which the next open detects.
    match worker.process.stop(STOP_GRACE).await {
        Ok(Stopped::Exited) => guards.worker_gone(),
        Ok(Stopped::Killed) => {
            eprintln!("ngr-agent: the worker did not stop in time and was killed");
            guards.worker_gone();
        }
        // The worker's state is unknown: with an unconfirmed link, the guards stay locked when
        // dropped, until this process exits.
        Err(error) if guards.link_unconfirmed() => {
            drop(guards);
            return Err(format!("the worker may still hold the VCI: {error}"));
        }
        Err(error) => eprintln!("ngr-agent: worker did not stop cleanly: {error}"),
    }
    // Released only now: either the worker was reaped, or its stop failed after a confirmed
    // close, so its link is closed either way.
    drop(guards);
    let state = result.map_err(|error| format!("job failed: {error}"))?;
    // serde_json would print such a value as `null`, which no longer reads back as the state.
    if holds_non_finite_float(&state) {
        return Err(
            "the final state holds a non-finite float, which JSON cannot represent".to_owned(),
        );
    }
    serde_json::to_string(&state).map_err(|error| format!("cannot print the result: {error}"))
}

/// Whether any value in `state` is an infinite or NaN `F64`.
fn holds_non_finite_float(state: &VmState) -> bool {
    let frames = state.call_stack.iter().flat_map(|frame| &frame.locals);
    state
        .stack
        .iter()
        .chain(
            state
                .locals
                .iter()
                .chain(&state.globals)
                .chain(frames)
                .flatten(),
        )
        .any(|value| matches!(value, Value::F64(number) if !number.is_finite()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A SHA-256 in lowercase hexadecimal.
    const HASH: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn parse(args: &[&str]) -> Result<RunArgs, String> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn parses_a_run_with_defaults() {
        assert_eq!(
            parse(&[
                "run",
                "--vci",
                "sim-vci",
                "--mode",
                "user",
                "--document",
                "v.dir"
            ]),
            Ok(RunArgs {
                api: Api::J2534,
                vci: "sim-vci".to_owned(),
                mode: OperatingMode::User,
                document: PathBuf::from("v.dir"),
                artifact: None,
                workers: None,
                locks: None,
                comm_log: None,
            })
        );
    }

    #[test]
    fn parses_every_option() {
        let locks = std::env::temp_dir().join("locks");
        let args = parse(&[
            "run",
            "--workers",
            "w",
            "--locks",
            locks.to_str().expect("a UTF-8 temp dir"),
            "--document",
            "v.dir",
            "--vci",
            "x",
            "--mode",
            "device",
            "--api",
            "dpdu",
            "--artifact",
            "a.bin",
            "--artifact-sha256",
            &"AB".repeat(32),
        ])
        .expect("valid arguments");
        assert_eq!(
            args.artifact,
            Some((PathBuf::from("a.bin"), [0xAB; SHA256_LEN]))
        );
        assert_eq!(args.api, Api::Dpdu);
        assert_eq!(args.vci, "x");
        assert_eq!(args.mode, OperatingMode::Device);
        assert_eq!(args.document, PathBuf::from("v.dir"));
        assert_eq!(args.workers, Some(PathBuf::from("w")));
        assert_eq!(args.locks, Some(locks));
        assert_eq!(args.comm_log, None);
    }

    #[test]
    fn parses_the_comm_log_directory_once() {
        let base = ["run", "--vci", "x", "--mode", "user", "--document", "d"];
        let with = |extra: &[&str]| parse(&[&base[..], extra].concat());
        assert_eq!(
            with(&["--comm-log", "logs"]).expect("valid").comm_log,
            Some(PathBuf::from("logs"))
        );
        assert!(with(&["--comm-log", "a", "--comm-log", "b"]).is_err());
        assert!(with(&["--comm-log"]).is_err());
    }

    #[test]
    fn a_run_mints_a_log_of_its_own_in_generation_zero() {
        let dir = std::env::temp_dir().join(format!("ngr-agent-comm-log-{}", std::process::id()));
        let setup = comm_log_setup(dir.clone()).expect("a log setup");
        assert_eq!(setup.key.generation, 0);
        let id = &setup.key.job_id.0;
        assert!(
            id.starts_with("run-") && id.ends_with(&format!("-{}", std::process::id())),
            "{id}"
        );
        let path = setup.path().expect("the ID names a file");
        assert_eq!(path.parent(), Some(dir.as_path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o700,
                "a new log directory is its owner's alone"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_bad_arguments() {
        for args in [
            &[][..],
            &["start"],
            &["run", "--mode", "user", "--document", "d.dir"],
            // One document, and one is required.
            &[
                "run",
                "--vci",
                "x",
                "--mode",
                "user",
                "--document",
                "d",
                "--document",
                "d",
            ],
            &["run", "--vci", "x", "--mode", "user"],
            &["run", "--vci"],
            // The mode has no default and only the exact spellings select one.
            &["run", "--vci", "x", "--document", "d"],
            &["run", "--vci", "x", "--document", "d", "--mode"],
            &["run", "--vci", "x", "--document", "d", "--mode", ""],
            &["run", "--vci", "x", "--document", "d", "--mode", "Device"],
            &["run", "--vci", "x", "--document", "d", "--mode", "service"],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
        let valid = ["run", "--vci", "x", "--mode", "user", "--document", "d"];
        for extra in [
            &["--verbose", "1"][..],
            &["--locks"],
            &["--locks", ""],
            &["--locks", "locks"],
            // Only the exact spellings select an API.
            &["--api"],
            &["--api", ""],
            &["--api", "J2534"],
            &["--api", "d-pdu"],
            // Both artifact options or neither, each once, the hash exactly 64 hex digits.
            &["--artifact", "a.bin"],
            &["--artifact-sha256", HASH],
            &[
                "--artifact",
                "a",
                "--artifact",
                "b",
                "--artifact-sha256",
                HASH,
            ],
            &[
                "--artifact",
                "a",
                "--artifact-sha256",
                HASH,
                "--artifact-sha256",
                HASH,
            ],
            &["--artifact", "a", "--artifact-sha256", &HASH[1..]],
            &["--artifact", "a", "--artifact-sha256", &format!("{HASH}0")],
            &[
                "--artifact",
                "a",
                "--artifact-sha256",
                &format!("g{}", &HASH[1..]),
            ],
            &["--artifact", "a", "--artifact-sha256", ""],
        ] {
            let args: Vec<&str> = valid.iter().chain(extra).copied().collect();
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    /// A job is always a document: the options that gave a bare program and its link are gone.
    #[test]
    fn the_retired_program_and_link_options_are_unknown() {
        let valid = ["run", "--vci", "x", "--mode", "user", "--document", "d"];
        for extra in [
            &["--program", "p.json"][..],
            &["--tx-id", "7E0"],
            &["--rx-id", "7E8"],
        ] {
            let args: Vec<&str> = valid.iter().chain(extra).copied().collect();
            let error = parse(&args).expect_err("a retired option");
            assert!(error.starts_with("unknown option"), "{error}");
        }
        let error = parse(&["run", "--vci", "x", "--mode", "user", "--program", "p.json"])
            .expect_err("a retired option");
        assert!(error.starts_with("unknown option"), "{error}");
    }

    #[test]
    fn finds_non_finite_floats_anywhere_in_the_state() {
        let finite = VmState {
            schema_version: 0,
            pc: 0,
            stack: vec![Value::F64(1.5), Value::Bytes(vec![1])],
            locals: vec![None, Some(Value::F64(-0.0))],
            globals: vec![Some(Value::I64(1))],
            call_stack: vec![diag_ir::Frame {
                return_pc: 0,
                locals: vec![Some(Value::F64(f64::MAX))],
            }],
            steps: 0,
        };
        assert!(!holds_non_finite_float(&finite));

        let mut stack = finite.clone();
        stack.stack.push(Value::F64(f64::INFINITY));
        let mut global = finite.clone();
        global.globals.push(Some(Value::F64(f64::NAN)));
        let mut frame = finite.clone();
        frame.call_stack[0]
            .locals
            .push(Some(Value::F64(f64::NEG_INFINITY)));
        for state in [stack, global, frame] {
            assert!(holds_non_finite_float(&state), "{state:?}");
        }
    }
}

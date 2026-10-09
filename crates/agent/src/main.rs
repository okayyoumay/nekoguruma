//! `ngr-agent`: the device-side agent (design 3.3). The job runner lives in the `agent` library.
//! The binary's one command, `ngr-agent run`, runs one IR program on a VCI and prints the final
//! VM state as JSON (`crates/agent/docs/ngr-agent.md`).

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use agent::guards::{GuardSetup, JobGuards};
use agent::launch::launch_j2534_worker;
use agent::policy::{build_ceiling, writes};
use agent::{JobLimits, LinkConfig, check_program, run_program};
use diag_ir::{Program, Value, VmState};
use worker_host::service::{LaunchOptions, WorkerLayout};

const USAGE: &str = "\
usage: ngr-agent run --vci <name> --program <file> [options]

Runs an IR program (diag-ir Program as JSON) on a J2534 v04.04 VCI and prints the final VM
state as JSON on stdout.

options:
  --vci <name>         J2534 library name the worker service resolves
  --program <file>     IR program file (JSON)
  --workers <dir>      worker binaries, laid out as <dir>/<ABI name>/<binary>
                       (default: the 'workers' directory next to ngr-agent)
  --locks <dir>        the device's lock directory for the VCI lock and the reprogramming
                       slot (default: the 'locks' directory next to ngr-agent)
  --tx-id <id>         physical request CAN ID, 11-bit hex (default 7E0)
  --rx-id <id>         response CAN ID, 11-bit hex (default 7E8)";

/// How often a job waiting for its guards tries again.
const GUARD_POLL: Duration = Duration::from_millis(10);

/// How long a stopping worker may take to close its link before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
struct RunArgs {
    vci: String,
    program: PathBuf,
    workers: Option<PathBuf>,
    locks: Option<PathBuf>,
    link: LinkConfig,
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
    let mut vci = None;
    let mut program = None;
    let mut workers = None;
    let mut locks = None;
    let mut tx_id = 0x7E0;
    let mut rx_id = 0x7E8;
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
            "--program" => program = Some(PathBuf::from(value)),
            "--workers" => workers = Some(PathBuf::from(value)),
            "--locks" if value.is_empty() => {
                return Err("--locks needs a directory".to_owned());
            }
            "--locks" => locks = Some(PathBuf::from(value)),
            "--tx-id" => tx_id = parse_can_id(&flag, &value)?,
            "--rx-id" => rx_id = parse_can_id(&flag, &value)?,
            _ => return Err(format!("unknown option {flag:?}")),
        }
    }
    let link = LinkConfig::iso15765(tx_id, rx_id);
    link.validate().map_err(|error| error.to_string())?;
    Ok(RunArgs {
        vci: vci.ok_or("--vci is required")?,
        program: program.ok_or("--program is required")?,
        workers,
        locks,
        link,
    })
}

/// A CAN ID in hex, with or without a `0x` prefix.
fn parse_can_id(flag: &str, value: &OsString) -> Result<u32, String> {
    let text = value.to_str().unwrap_or_default();
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    let invalid = || format!("{flag} {value:?} is not a hex CAN ID");
    // `from_str_radix` would also take a sign.
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    u32::from_str_radix(digits, 16).map_err(|_| invalid())
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

/// Runs the job and returns the final VM state as JSON.
async fn run(args: RunArgs) -> Result<String, String> {
    let text = std::fs::read_to_string(&args.program)
        .map_err(|error| format!("cannot read {}: {error}", args.program.display()))?;
    let program: Program = serde_json::from_str(&text)
        .map_err(|error| format!("{} is not an IR program: {error}", args.program.display()))?;
    // A program the job would refuse does not need a worker.
    check_program(&program, build_ceiling()).map_err(|error| format!("job failed: {error}"))?;
    // The per-VCI lock, and the reprogramming slot for a program that writes (design 8.8.1). The
    // wait polls a file lock, so it runs off the runtime's threads; nothing cancels it here.
    // An absolute path, so runs from different working directories share one lock set.
    let dir = match args.locks {
        Some(dir) => std::path::absolute(&dir)
            .map_err(|error| format!("--locks {}: {error}", dir.display()))?,
        None => default_locks()?,
    };
    let setup = GuardSetup {
        dir,
        vci: args.vci.clone(),
    };
    let writes = writes(&program);
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
    let worker = launch_j2534_worker(&args.vci, &workers, &LaunchOptions::default())
        .await
        .map_err(|error| error.to_string())?;
    let (result, guards) = run_program(
        worker.client,
        &args.link,
        program,
        JobLimits::default(),
        guards,
    )
    .await;
    // The job has closed its link whatever the result; a failed stop does not change it.
    if let Err(error) = worker.process.stop(STOP_GRACE).await {
        eprintln!("ngr-agent: worker did not stop cleanly: {error}");
    }
    // Released only now, with the worker gone: the VCI is free for the next job.
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

    fn parse(args: &[&str]) -> Result<RunArgs, String> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn parses_a_run_with_defaults() {
        assert_eq!(
            parse(&["run", "--vci", "sim-vci", "--program", "p.json"]),
            Ok(RunArgs {
                vci: "sim-vci".to_owned(),
                program: PathBuf::from("p.json"),
                workers: None,
                locks: None,
                link: LinkConfig::iso15765(0x7E0, 0x7E8),
            })
        );
    }

    #[test]
    fn parses_every_option() {
        let args = parse(&[
            "run",
            "--workers",
            "w",
            "--locks",
            "l",
            "--tx-id",
            "0x7DF",
            "--rx-id",
            "7e9",
            "--program",
            "p.json",
            "--vci",
            "x",
        ])
        .expect("valid arguments");
        assert_eq!(args.workers, Some(PathBuf::from("w")));
        assert_eq!(args.locks, Some(PathBuf::from("l")));
        assert_eq!((args.link.tx_id, args.link.rx_id), (0x7DF, 0x7E9));
    }

    #[test]
    fn refuses_bad_arguments() {
        for args in [
            &[][..],
            &["start"],
            &["run", "--program", "p.json"],
            &["run", "--vci", "x"],
            &["run", "--vci"],
            &["run", "--vci", "x", "--program", "p", "--tx-id", "7G0"],
            &["run", "--vci", "x", "--program", "p", "--tx-id", "+7E0"],
            &["run", "--vci", "x", "--program", "p", "--tx-id", "0x"],
            // 29-bit IDs need the link to set the ID format.
            &["run", "--vci", "x", "--program", "p", "--rx-id", "18DAF110"],
            &["run", "--vci", "x", "--program", "p", "--verbose", "1"],
            &["run", "--vci", "x", "--program", "p", "--locks"],
            &["run", "--vci", "x", "--program", "p", "--locks", ""],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
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

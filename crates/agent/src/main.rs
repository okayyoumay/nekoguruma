//! `ngr-agent`: the device-side agent (design 3.3). The job runner lives in the `agent` library.
//! The binary's one command, `ngr-agent run`, runs one IR program on a VCI and prints the final
//! VM state as JSON (`crates/agent/docs/ngr-agent.md`).

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use agent::launch::launch_j2534_worker;
use agent::{JobLimits, LinkConfig, check_program, run_program};
use diag_ir::Program;
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
  --tx-id <id>         physical request CAN ID, 11-bit hex (default 7E0)
  --rx-id <id>         response CAN ID, 11-bit hex (default 7E8)";

/// How long a stopping worker may take to close its link before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
struct RunArgs {
    vci: String,
    program: PathBuf,
    workers: Option<PathBuf>,
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
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot locate ngr-agent: {error}; pass --workers"))?;
    let dir = exe
        .parent()
        .ok_or("cannot locate the directory of ngr-agent; pass --workers")?;
    Ok(dir.join("workers"))
}

/// Runs the job and returns the final VM state as JSON.
async fn run(args: RunArgs) -> Result<String, String> {
    let text = std::fs::read_to_string(&args.program)
        .map_err(|error| format!("cannot read {}: {error}", args.program.display()))?;
    let program: Program = serde_json::from_str(&text)
        .map_err(|error| format!("{} is not an IR program: {error}", args.program.display()))?;
    // A program the job would refuse does not need a worker.
    check_program(&program).map_err(|error| format!("job failed: {error}"))?;
    let workers = WorkerLayout {
        root: match args.workers {
            Some(root) => root,
            None => default_workers()?,
        },
    };
    let worker = launch_j2534_worker(&args.vci, &workers, &LaunchOptions::default())
        .await
        .map_err(|error| error.to_string())?;
    let result = run_program(worker.client, &args.link, program, JobLimits::default()).await;
    // The job has closed its link whatever the result; a failed stop does not change it.
    if let Err(error) = worker.process.stop(STOP_GRACE).await {
        eprintln!("ngr-agent: worker did not stop cleanly: {error}");
    }
    let state = result.map_err(|error| format!("job failed: {error}"))?;
    serde_json::to_string(&state).map_err(|error| format!("cannot print the result: {error}"))
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
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }
}

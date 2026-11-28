//! `ngr-stand-in-ecu`: serves a simulated ECU over a SocketCAN interface (see the library
//! documentation and `crates/sim-ecu/docs/simulated-ecu.md`).

use std::process::ExitCode;

use ngr_stand_in_ecu::args::{self, Command, USAGE};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("ngr-stand-in-ecu: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    match args::parse(std::env::args().skip(1)).map_err(|e| format!("{e}\n\n{USAGE}"))? {
        Command::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Command::Serve(args) => serve(&args),
    }
}

#[cfg(target_os = "linux")]
fn serve(args: &args::Args) -> Result<(), String> {
    use ngr_stand_in_ecu::isotp::{IsoTpTransport, stop_on_signals};
    use ngr_stand_in_ecu::serve::Station;

    // The transport and the signals come first: a start that fails there must leave the state
    // file as it was, with no new file and no extra power cycle.
    let mut transport = IsoTpTransport::open(&args.interface, &args.ids).map_err(|e| {
        format!(
            "cannot open the ISO-TP sockets on {} (is the can-isotp module loaded and the \
             interface up?): {e}",
            args.interface
        )
    })?;
    let stop = stop_on_signals().map_err(|e| format!("cannot install the stop signals: {e}"))?;
    let mut station =
        Station::start(args.config.as_deref(), args.state.as_deref()).map_err(|e| e.to_string())?;
    // A script (or a test) that starts the process waits for this line before it sends.
    println!("ngr-stand-in-ecu: serving on {}", args.interface);
    station.run(&mut transport, stop).map_err(|e| e.to_string())
}

#[cfg(not(target_os = "linux"))]
fn serve(_args: &args::Args) -> Result<(), String> {
    Err("Linux only: the stand-in ECU needs the kernel's SocketCAN ISO-TP sockets".to_owned())
}

//! `ngr-hw-checks list`: lists the hardware checks the profile `NGR_HW_PROFILE` names permits,
//! without running any (design 13.5, procedure step 2).

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (Some("list"), None) => match ngr_hw_checks::profile::from_env() {
            Ok(profile) => {
                print!(
                    "{}",
                    ngr_hw_checks::listing(&profile, ngr_hw_checks::CHECKS)
                );
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("usage: ngr-hw-checks list");
            ExitCode::from(2)
        }
    }
}

//! The real-hardware check harness (design 13.5, ADR-275).
//!
//! - [`profile`]: the hardware profile `NGR_HW_PROFILE` names, checked against its class
//! - [`device`]: the module match, whether the device a worker reports is the profile's
//! - [`open`]: starting the profile's worker, running the module match and opening the device
//! - [`report`]: the report of a run, written to a directory only the running user can read
//! - [`guard`]: the decision core, one pure function over the profile, the bound links and a worker
//!   request, and [`guard::GuardedHandle`], the handle a check gets in place of a worker client
//!   (ADR-333)
//! - [`checks`]: the bodies of the hardware checks, over guarded handles, so CI runs them on a mock
//! - [`teardown`]: the steps a run registers as it acquires resources and runs in reverse, once
//! - [`CheckDecl`]: what a check needs, as static data the harness reads without running it
//! - [`permits`] and [`listing`]: which checks a profile permits
//! - [`begin`]: the first call of every `hw_` test, which fails without a permitting profile
//!
//! A hardware check is a Rust test named `hw_...`, marked `#[ignore]` with a reason naming design
//! 13.5, that starts with [`begin`] on its declaration. Its declaration is added to [`CHECKS`],
//! so `cargo run -p ngr-hw-checks -- list` lists it. The checks run with
//! `cargo nextest run --profile hardware --run-ignored=only`; CI never runs them.

pub mod checks;
pub mod comparam;
pub mod device;
pub mod guard;
pub mod open;
pub mod profile;
pub mod report;
pub mod teardown;

pub use profile::{Class, Operation, Profile, ProfileError};

/// What a check needs (design 13.5, ADR-275 item 3): the classes it can run on and the operations
/// beyond its class's reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckDecl {
    /// The test's name, starting with `hw_`.
    pub name: &'static str,
    pub classes: &'static [Class],
    pub operations: &'static [Operation],
}

/// The declarations of every hardware check, which [`listing`] reads. A new `hw_` test adds its
/// declaration here.
pub const CHECKS: &[CheckDecl] = &[
    PROFILE_CHECK,
    T0_LOOPBACK_CHECK,
    T2_T3_READ_CHECK,
    T2_TIMING_CHECK,
];

/// `hw_the_profile_permits_a_check`: the profile reads and permits a check that needs nothing,
/// on every class. It opens no device; it shows that the `hardware` run finds the profile.
pub const PROFILE_CHECK: CheckDecl = CheckDecl {
    name: "hw_the_profile_permits_a_check",
    classes: &[Class::T0, Class::T1, Class::T2, Class::T3],
    operations: &[],
};

/// `hw_t0_two_endpoint_loopback`: on a T0 loopback of two VCI ends, or of a VCI end and a
/// SocketCAN end on Linux, each end sends single frames that the other must receive exactly, in
/// both directions (`checks::t0_loopback`). It needs a T0 profile and no operation beyond the
/// class's.
pub const T0_LOOPBACK_CHECK: CheckDecl = CheckDecl {
    name: "hw_t0_two_endpoint_loopback",
    classes: &[Class::T0],
    operations: &[],
};

/// `hw_t2_t3_read`: on a bench ECU or a vehicle, the profile's definition (`[read]`) identifies
/// the ECU's variant and runs its read procedure through the agent's job runner on the guarded
/// client (`checks::t2_t3_read`). It needs a T2 or T3 profile and no operation beyond reads.
pub const T2_T3_READ_CHECK: CheckDecl = CheckDecl {
    name: "hw_t2_t3_read",
    classes: &[Class::T2, Class::T3],
    operations: &[],
};

/// `hw_t2_timing`: on a bench ECU, the P2 and P2* the ECU reports and the agent's round trips of
/// a few requests, compared with the values `sim-ecu` uses (`checks::t2_timing`). It needs a T2
/// profile and no operation beyond reads.
pub const T2_TIMING_CHECK: CheckDecl = CheckDecl {
    name: "hw_t2_timing",
    classes: &[Class::T2],
    operations: &[],
};

/// Why a profile does not permit a check.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("the check runs on {needs:?}, not on the profile's {class:?}")]
    Class { class: Class, needs: Vec<Class> },
    #[error("the check needs {0:?}, which the profile does not allow")]
    Operation(Operation),
}

/// Whether `profile` permits `check`: the profile's class is one the check runs on, and the
/// profile allows every operation the check needs.
pub fn permits(profile: &Profile, check: &CheckDecl) -> Result<(), Refusal> {
    if !check.classes.contains(&profile.class) {
        return Err(Refusal::Class {
            class: profile.class,
            needs: check.classes.to_vec(),
        });
    }
    match check.operations.iter().find(|&&op| !profile.allows(op)) {
        Some(&operation) => Err(Refusal::Operation(operation)),
        None => Ok(()),
    }
}

/// One line per check: its name, and whether `profile` permits it or why not. Nothing runs.
pub fn listing(profile: &Profile, checks: &[CheckDecl]) -> String {
    let mut out = String::new();
    for check in checks {
        let verdict = match permits(profile, check) {
            Ok(()) => "permitted".to_owned(),
            Err(refusal) => format!("refused: {refusal}"),
        };
        out.push_str(&format!("{}: {verdict}\n", check.name));
    }
    out
}

/// Why a hardware check cannot start.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StartError {
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error("{check}: {refusal}")]
    Refused {
        check: &'static str,
        refusal: Refusal,
    },
}

/// The profile `check` runs with, from `profile` (the result of reading the profile). A hardware
/// check never passes without one: running it was a request (ADR-275 item 2).
pub fn start(
    check: &CheckDecl,
    profile: Result<Profile, ProfileError>,
) -> Result<Profile, StartError> {
    let profile = profile?;
    permits(&profile, check).map_err(|refusal| StartError::Refused {
        check: check.name,
        refusal,
    })?;
    Ok(profile)
}

/// The first call of a `hw_` test: the profile `NGR_HW_PROFILE` names, if it permits `check`,
/// and the run's [`report::RunGuard`], which writes the report when the test ends. Panics
/// otherwise, so the test fails before it reaches any device, after recording the failure: a
/// profile that cannot be read leaves a report with the class unknown and every identifier
/// masked, and one that does not permit the check leaves a report with the check refused.
pub fn begin(check: &CheckDecl) -> (Profile, report::RunGuard) {
    let profile = match profile::from_env() {
        Ok(profile) => profile,
        Err(error) => {
            let mut run = report::RunGuard::start_without_profile(check.name);
            // The error names the profile's path and can quote its text, which may hold serial
            // numbers no profile tells the masker about; the report keeps a fixed message and
            // the details go to the test's output.
            run.finish(
                report::Outcome::Failed,
                "the hardware profile could not be read or is invalid; see the test output",
            );
            panic!("{error}");
        }
    };
    let mut run = report::RunGuard::start(&profile, check.name);
    if let Err(error) = start(check, Ok(profile.clone())) {
        run.finish(report::Outcome::Refused, &error.to_string());
        panic!("{error}");
    }
    (profile, run)
}

#[cfg(test)]
mod tests;

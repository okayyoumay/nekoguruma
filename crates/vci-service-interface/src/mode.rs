//! The operating mode a worker is started in (design 6.2, 7.2; ADR-281 Decision item 2).
//!
//! A device runs in exactly one of two modes, fixed at installation. The agent passes its
//! device's mode to every worker it starts, as the `mode` query parameter of the startup
//! argument (`<scheme>:<library name>?mode=<device|user>&...`). The worker does not infer the
//! mode and no configuration file sets it: a missing or unknown value is a startup error. Design
//! 7.2 gives the mode its meaning for a failed library check: device mode refuses the library,
//! user mode reports the findings and loads it. The workers record the mode; their library
//! loading does not consult it.
//!
//! The agent (`worker-host`) writes the parameter and both worker services parse it, so the
//! spelling lives here, in the crate both sides already share for the gRPC interface.

use std::fmt;
use std::str::FromStr;

/// Name of the startup argument's query parameter that carries the mode.
pub const MODE_QUERY_KEY: &str = "mode";

/// A device's operating mode (design 6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperatingMode {
    /// Unattended devices; design 7.2 refuses a library that fails a check in this mode.
    Device,
    /// Interactive devices; design 7.2 reports a failed check in this mode and loads the library.
    User,
}

impl OperatingMode {
    /// The value written in the startup argument.
    pub fn as_str(self) -> &'static str {
        match self {
            OperatingMode::Device => "device",
            OperatingMode::User => "user",
        }
    }
}

impl fmt::Display for OperatingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A mode value that is neither `device` nor `user`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownMode(pub String);

impl fmt::Display for UnknownMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown operating mode {:?} (expected \"device\" or \"user\")",
            self.0
        )
    }
}

impl std::error::Error for UnknownMode {}

impl FromStr for OperatingMode {
    type Err = UnknownMode;

    /// Parses the exact lowercase spelling; anything else, including other case, is refused so
    /// that a typo cannot select a mode.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "device" => Ok(OperatingMode::Device),
            "user" => Ok(OperatingMode::User),
            other => Err(UnknownMode(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_both_modes() {
        for mode in [OperatingMode::Device, OperatingMode::User] {
            assert_eq!(mode.as_str().parse::<OperatingMode>(), Ok(mode));
            assert_eq!(mode.to_string(), mode.as_str());
        }
    }

    #[test]
    fn refuses_anything_but_the_exact_spelling() {
        for value in ["", "Device", "USER", " user", "devices", "admin"] {
            assert_eq!(
                value.parse::<OperatingMode>(),
                Err(UnknownMode(value.to_owned())),
                "{value:?}"
            );
        }
    }
}

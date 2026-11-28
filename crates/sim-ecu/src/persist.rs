//! The files a simulated ECU is started from: its configuration (JSON) and the state file that
//! keeps it across processes (ADR-241). `sim-vci` and `ngr-stand-in-ecu` read and write them the
//! same way.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{EcuConfig, EcuSnapshot, SimEcu, SystemClock};

/// Format of the state file; a file of another version is refused. postcard has no field names,
/// so any change to the encoded `EcuSnapshot` (version 2: `EcuConfig::startup_ms` and the time
/// left of the ECU's startup, ADR-265; version 3: the faults waiting for a request,
/// `SimEcu::inject_for_request`; version 4: `EcuConfig::p2_server_max_ms` and
/// `p2_star_server_max_ms`; version 5: a held session, `SimEcu::hold_session`; version 6:
/// `EcuConfig::security_key_mask` and `EcuConfig::dids`; version 7: the matches a fault for a
/// request still waits for, `SimEcu::inject_for_nth_request`) needs a new version.
pub(crate) const STATE_FILE_VERSION: u32 = 7;

/// The contents of a state file (postcard).
#[derive(Serialize, Deserialize)]
pub(crate) struct StateFile {
    pub(crate) version: u32,
    /// Wall-clock time of the write, in milliseconds since the Unix epoch: the time until the
    /// next process reads it counts against the ECU's timers.
    pub(crate) saved_at_ms: u64,
    pub(crate) ecu: EcuSnapshot,
}

pub(crate) fn unix_ms(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis().try_into().unwrap_or(u64::MAX))
}

/// The configuration of the simulated ECU when no configuration file is given.
pub fn default_ecu_config() -> EcuConfig {
    EcuConfig {
        vin: "NGRSIMECU00000001".into(),
        part_number: "NGR-SIM-ECU".into(),
        sw_version: "1.0.0".into(),
        ..Default::default()
    }
}

/// The [`EcuConfig`] in the JSON file at `path`, checked with [`EcuConfig::validate`]. The error
/// names what failed and keeps its cause in the text.
pub fn read_ecu_config(path: &Path) -> Result<EcuConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("configuration file {} unreadable: {e}", path.display()))?;
    let config: EcuConfig = serde_json::from_str(&text)
        .map_err(|e| format!("configuration file {} does not parse: {e}", path.display()))?;
    config
        .validate()
        .map_err(|e| format!("configuration file {} is invalid: {e}", path.display()))?;
    Ok(config)
}

/// The ECU the state file at `path` holds, with the time since it was written counted against
/// its timers: `Ok(None)` if there is no such file, `Err` if it cannot be read (for any other
/// reason than not existing), is not a state file of this version, or holds a state the ECU
/// could not have been in.
pub fn load_ecu_state(path: &Path) -> Result<Option<SimEcu>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("state file unreadable: {error}")),
    };
    // The version comes first and is checked before the rest is decoded: a file of another
    // version has another layout, which could otherwise decode into wrong fields.
    let (version, _) = postcard::take_from_bytes::<u32>(&bytes)
        .map_err(|e| format!("state file version unreadable: {e}"))?;
    if version != STATE_FILE_VERSION {
        return Err(format!(
            "state file has version {version}, not {STATE_FILE_VERSION}"
        ));
    }
    let state: StateFile =
        postcard::from_bytes(&bytes).map_err(|e| format!("state file does not decode: {e}"))?;
    // A wall clock set back since the write counts as no time passed.
    let elapsed = unix_ms(SystemTime::now()).saturating_sub(state.saved_at_ms);
    SimEcu::restore(
        state.ecu,
        SystemClock::default(),
        Duration::from_millis(elapsed),
    )
    .map(Some)
    .map_err(|e| format!("state file holds an invalid snapshot: {e}"))
}

/// Writes `ecu`'s state to `path`, through a temporary file renamed over it, so a process killed
/// while writing leaves the previous state.
pub fn save_ecu_state(path: &Path, ecu: &mut SimEcu) -> std::io::Result<()> {
    let state = StateFile {
        version: STATE_FILE_VERSION,
        saved_at_ms: unix_ms(SystemTime::now()),
        ecu: ecu.snapshot(),
    };
    let bytes = postcard::to_allocvec(&state).map_err(std::io::Error::other)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, bytes)?;
    // On Windows another process (a virus scanner, for one) can hold the new file open for a
    // moment, and replacing it then fails; try again a few times before giving up.
    let mut attempts = 0;
    loop {
        match std::fs::rename(&tmp, path) {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied && attempts < 10 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests;

//! The module match (design 13.5, ADR-275 item 4): whether the device a worker reports is the
//! one the profile's `[vci]` names, decided from the worker's answers alone so that a check can
//! refuse a wrong device before it creates a link.
//!
//! The answers are those of the worker's `GetModuleIds` (each module's `vendor_module_name` and
//! `vendor_additional_info`) and `GetVersion` (`VersionData`), and for J2534 the `modules` entries
//! of the worker's configuration (ADR-281). Reading them from a worker is the harness's part.

use crate::profile::{Api, Vci};

/// One module as the worker's `GetModuleIds` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedModule {
    /// `vendor_module_name`: the device name or serial number the library reports, or for J2534
    /// the label of the configured entry (`j2534-0404-service`, ADR-281).
    pub name: String,
    /// `vendor_additional_info`: for J2534 the library name the worker loaded.
    pub additional_info: String,
}

/// A `modules` entry of a J2534 worker's configuration (ADR-281).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredModule {
    pub label: String,
    pub pname: String,
}

/// The fields of the worker's `GetVersion` answer the match reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReportedVersion {
    /// `hw_name`: for J2534 the firmware version string the device reports.
    pub hw_name: String,
    /// `fw_name`: for J2534 the library (DLL) version string.
    pub fw_name: String,
    /// `fw_version`: for the D-PDU API the firmware version, coded as major, minor and revision
    /// in the three high bytes (ISO 22900-2 (2022) 10.1.4.15).
    pub fw_version: u32,
    /// `pdu_api_sw_version`: for the D-PDU API the library's version, coded the same way.
    pub pdu_api_sw_version: u32,
}

/// Why the attached device is not the profile's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MatchRefusal {
    #[error("no module the worker reports is {module:?}")]
    NoModule { module: String },
    #[error("{count} modules the worker reports are {module:?}")]
    SeveralModules { module: String, count: usize },
    #[error("no configured `modules` entry has the label {label:?}")]
    UnknownLabel { label: String },
    #[error(
        "{count} `modules` entries are configured; a J2534 check needs its module to be the only one"
    )]
    SeveralConfigured { count: usize },
    #[error("the `modules` entry {label:?} names the device {configured:?}, not {profile:?}")]
    PnameDiffers {
        label: String,
        configured: String,
        profile: String,
    },
    #[error("the worker loaded the library {reported:?}, not {profile:?}")]
    LibraryDiffers { reported: String, profile: String },
    #[error("the device reports the firmware version {reported:?}, not {profile:?}")]
    FirmwareDiffers { reported: String, profile: String },
    #[error("the device reports the library version {reported:?}, not {profile:?}")]
    LibraryVersionDiffers { reported: String, profile: String },
}

/// A version number coded as ISO 22900-2 (2022) 10.1.4.15 codes it, as `major.minor.revision`.
pub fn coded_version(version: u32) -> String {
    let [major, minor, revision, _] = version.to_be_bytes();
    format!("{major}.{minor}.{revision}")
}

/// The index in `reported` of the module the profile names, or why the device is not it.
///
/// - D-PDU API: exactly one module whose name or additional information is the profile's
///   `module`.
/// - J2534: the configured `modules` entry with the profile's `label` must be the only entry, so
///   a configured module that is not attached cannot be taken for the attached one (ADR-275 item
///   4), and its `pname` must be the profile's; exactly one reported module carries that label,
///   from the profile's library.
pub fn select_module(
    vci: &Vci,
    reported: &[ReportedModule],
    configured: &[ConfiguredModule],
) -> Result<usize, MatchRefusal> {
    let matches: Vec<usize> = match vci.api {
        Api::Iso22900 => reported
            .iter()
            .enumerate()
            .filter(|(_, m)| m.name == vci.module || m.additional_info == vci.module)
            .map(|(at, _)| at)
            .collect(),
        Api::J2534V0404 => {
            let label = vci.label.clone().unwrap_or_default();
            let entry = configured
                .iter()
                .find(|entry| entry.label == label)
                .ok_or_else(|| MatchRefusal::UnknownLabel {
                    label: label.clone(),
                })?;
            if configured.len() > 1 {
                return Err(MatchRefusal::SeveralConfigured {
                    count: configured.len(),
                });
            }
            let pname = vci.pname.clone().unwrap_or_default();
            if entry.pname != pname {
                return Err(MatchRefusal::PnameDiffers {
                    label,
                    configured: entry.pname.clone(),
                    profile: pname,
                });
            }
            let matches: Vec<usize> = reported
                .iter()
                .enumerate()
                .filter(|(_, m)| m.name == label)
                .map(|(at, _)| at)
                .collect();
            if let [at] = matches[..]
                && reported[at].additional_info != vci.library
            {
                return Err(MatchRefusal::LibraryDiffers {
                    reported: reported[at].additional_info.clone(),
                    profile: vci.library.clone(),
                });
            }
            matches
        }
    };
    let module = match vci.api {
        Api::Iso22900 => vci.module.clone(),
        Api::J2534V0404 => vci.label.clone().unwrap_or_default(),
    };
    match matches[..] {
        [] => Err(MatchRefusal::NoModule { module }),
        [at] => Ok(at),
        _ => Err(MatchRefusal::SeveralModules {
            module,
            count: matches.len(),
        }),
    }
}

/// Whether the opened device reports the profile's firmware and library versions. J2534 reports
/// them as strings (`hw_name` and `fw_name` of `j2534-0404-service`), compared exactly; the D-PDU
/// API as coded numbers, compared as `major.minor.revision`.
pub fn check_versions(vci: &Vci, version: &ReportedVersion) -> Result<(), MatchRefusal> {
    let (firmware, library) = match vci.api {
        Api::J2534V0404 => (version.hw_name.clone(), version.fw_name.clone()),
        Api::Iso22900 => (
            coded_version(version.fw_version),
            coded_version(version.pdu_api_sw_version),
        ),
    };
    if firmware != vci.firmware_version {
        return Err(MatchRefusal::FirmwareDiffers {
            reported: firmware,
            profile: vci.firmware_version.clone(),
        });
    }
    if library != vci.library_version {
        return Err(MatchRefusal::LibraryVersionDiffers {
            reported: library,
            profile: vci.library_version.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "device_tests.rs"]
mod tests;

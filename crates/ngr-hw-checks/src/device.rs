//! The module match (design 13.5, ADR-275 item 4): whether the device a worker reports is the
//! one the profile's `[vci]` names, decided from the worker's answers alone so that a check can
//! refuse a wrong device before it creates a link.
//!
//! The answers are those of the worker's `GetModuleIds` (each module's `vendor_module_name` and
//! `vendor_additional_info`) and `GetVersion` (`VersionData`), and for J2534 the `modules` entries
//! of the worker's configuration (ADR-281). [`crate::open::open_device`] reads them from a worker
//! and applies the match.

use crate::profile::{Api, Vci};

/// One module as the worker's `GetModuleIds` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedModule {
    /// `vendor_module_name`: for the D-PDU API the library's identification of the module, a list
    /// of `attribute='value'` pairs (ISO 22900-2 (2022) 10.1.4.6) or empty; for J2534 the label of
    /// the configured entry (`j2534-0404-service`, ADR-281).
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
    #[error("the profile's {what} is empty")]
    Unnamed { what: &'static str },
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

/// Whether `version` is written as [`coded_version`] writes one: three decimal numbers from 0 to
/// 255, separated by dots, with no sign and no leading zero, so that a matching device's version
/// reads the same.
pub fn is_coded_version(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            part.parse::<u8>()
                .is_ok_and(|byte| byte.to_string() == *part)
        })
}

/// The values of a D-PDU API vendor string's `attribute='value'` pairs (ISO 22900-2 (2022)
/// 10.1.4.6), in order. Text that does not form a pair is skipped.
pub fn attribute_values(vendor: &str) -> Vec<&str> {
    let mut values = Vec::new();
    let mut rest = vendor;
    while let Some(equals) = rest.find('=') {
        let after = rest[equals + 1..].trim_start();
        let Some(quoted) = after.strip_prefix('\'') else {
            rest = after;
            continue;
        };
        let Some(end) = quoted.find('\'') else {
            break;
        };
        values.push(&quoted[..end]);
        rest = &quoted[end + 1..];
    }
    values
}

/// Whether a D-PDU API module's `vendor_module_name` identifies the profile's `module`: the
/// whole string, or the value of one of its attributes, such as a serial number or a friendly
/// name, equals it. Surrounding spaces do not count.
fn identifies(vendor: &str, module: &str) -> bool {
    vendor.trim() == module
        || attribute_values(vendor)
            .iter()
            .any(|value| value.trim() == module)
}

/// The index in `reported` of the module the profile names, or why the device is not it.
///
/// - D-PDU API: exactly one module whose `vendor_module_name` identifies the profile's
///   `module` ([`identifies`]). The additional information is not read: it describes the
///   connection, which can change while the module stays the same.
/// - J2534: the configured `modules` entry with the profile's `label` must be the only entry, so
///   a configured module that is not attached cannot be taken for the attached one (ADR-275 item
///   4), and its `pname` must be the profile's; the worker reports exactly one module, which
///   carries that label and comes from the profile's library. Names are compared without their surrounding spaces.
pub fn select_module(
    vci: &Vci,
    reported: &[ReportedModule],
    configured: &[ConfiguredModule],
) -> Result<usize, MatchRefusal> {
    let matches: Vec<usize> = match vci.api {
        Api::Iso22900 => {
            let module = vci.module.trim();
            if module.is_empty() {
                return Err(MatchRefusal::Unnamed { what: "module" });
            }
            reported
                .iter()
                .enumerate()
                .filter(|(_, m)| identifies(&m.name, module))
                .map(|(at, _)| at)
                .collect()
        }
        Api::J2534V0404 => {
            let label = vci.label.as_deref().unwrap_or_default().trim().to_owned();
            if label.is_empty() {
                return Err(MatchRefusal::Unnamed { what: "label" });
            }
            let entry = configured
                .iter()
                .find(|entry| entry.label.trim() == label)
                .ok_or_else(|| MatchRefusal::UnknownLabel {
                    label: label.clone(),
                })?;
            if configured.len() > 1 {
                return Err(MatchRefusal::SeveralConfigured {
                    count: configured.len(),
                });
            }
            let pname = vci.pname.as_deref().unwrap_or_default().trim().to_owned();
            if entry.pname.trim() != pname {
                return Err(MatchRefusal::PnameDiffers {
                    label,
                    configured: entry.pname.clone(),
                    profile: pname,
                });
            }
            // The harness reads the `modules` entries from its own view of the configuration,
            // which can differ from the worker's, so the worker must also report exactly one
            // module, as the agent requires of a worker (ADR-235 item 4).
            if reported.len() > 1 {
                return Err(MatchRefusal::SeveralModules {
                    module: label,
                    count: reported.len(),
                });
            }
            let matches: Vec<usize> = reported
                .iter()
                .enumerate()
                .filter(|(_, m)| m.name.trim() == label)
                .map(|(at, _)| at)
                .collect();
            if let [at] = matches[..]
                && reported[at].additional_info.trim() != vci.library.trim()
            {
                return Err(MatchRefusal::LibraryDiffers {
                    reported: reported[at].additional_info.clone(),
                    profile: vci.library.trim().to_owned(),
                });
            }
            matches
        }
    };
    let module = match vci.api {
        Api::Iso22900 => vci.module.trim().to_owned(),
        Api::J2534V0404 => vci.label.as_deref().unwrap_or_default().trim().to_owned(),
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
/// them as strings (`hw_name` and `fw_name` of `j2534-0404-service`), compared without
/// surrounding spaces, which a library may pad its fixed-size buffers with; the D-PDU API as
/// coded numbers, compared as `major.minor.revision`.
pub fn check_versions(vci: &Vci, version: &ReportedVersion) -> Result<(), MatchRefusal> {
    let (firmware, library) = match vci.api {
        Api::J2534V0404 => (
            version.hw_name.trim().to_owned(),
            version.fw_name.trim().to_owned(),
        ),
        Api::Iso22900 => (
            coded_version(version.fw_version),
            coded_version(version.pdu_api_sw_version),
        ),
    };
    if firmware != vci.firmware_version.trim() {
        return Err(MatchRefusal::FirmwareDiffers {
            reported: firmware,
            profile: vci.firmware_version.trim().to_owned(),
        });
    }
    if library != vci.library_version.trim() {
        return Err(MatchRefusal::LibraryVersionDiffers {
            reported: library,
            profile: vci.library_version.trim().to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "device_tests.rs"]
mod tests;

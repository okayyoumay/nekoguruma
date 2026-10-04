//! J2534 v04.04 discovery from the Windows registry (7.1, J2534-1 section 9.2).
//!
//! Each installed device has one key under `HKLM\SOFTWARE\PassThruSupport.04.04` holding
//! `FunctionLibrary` (absolute DLL path), display strings and per-protocol channel counts.

use std::path::PathBuf;

use crate::{DiscoveryError, RegistryView};

pub const REGISTRY_KEY: &str = r"SOFTWARE\PassThruSupport.04.04";

/// Protocol values a device key may carry (DWORD: number of simultaneous channels).
/// Informational only: the hardware configuration may have changed since installation.
pub const PROTOCOL_VALUES: &[&str] = &[
    "J1850VPW",
    "J1850PWM",
    "ISO9141",
    "ISO14230",
    "CAN",
    "ISO15765",
    "SCI_A_ENGINE",
    "SCI_A_TRANS",
    "SCI_B_ENGINE",
    "SCI_B_TRANS",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct J2534Device {
    /// Name of the device key (by convention "Vendor - Device").
    pub key_name: String,
    pub vendor: Option<String>,
    pub name: Option<String>,
    /// As written by the installer. Environment variables are not expanded here.
    pub function_library: PathBuf,
    pub config_application: Option<PathBuf>,
    /// (protocol value name, channel count) for every protocol with a non-zero count.
    pub protocols: Vec<(String, u32)>,
    pub view: RegistryView,
}

/// Enumerates devices under one registry view, sorted by key name.
/// A missing `PassThruSupport.04.04` key yields an empty list; keys without a
/// `FunctionLibrary` value are skipped.
#[cfg(windows)]
pub fn enumerate(view: RegistryView) -> Result<Vec<J2534Device>, DiscoveryError> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};

    let flags = KEY_READ | view.flag();
    let root = match RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(REGISTRY_KEY, flags)
    {
        Ok(key) => key,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut devices = Vec::new();
    for key_name in root.enum_keys() {
        let key_name = key_name?;
        let Ok(key) = root.open_subkey_with_flags(&key_name, flags) else {
            continue;
        };
        let string = |name: &str| {
            key.get_value::<String, _>(name)
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let Some(function_library) = string("FunctionLibrary") else {
            continue;
        };
        let protocols = PROTOCOL_VALUES
            .iter()
            .filter_map(|&p| match key.get_value::<u32, _>(p) {
                Ok(n) if n > 0 => Some((p.to_owned(), n)),
                _ => None,
            })
            .collect();
        devices.push(J2534Device {
            vendor: string("Vendor"),
            name: string("Name"),
            function_library: function_library.into(),
            config_application: string("ConfigApplication").map(PathBuf::from),
            protocols,
            view,
            key_name,
        });
    }
    devices.sort_by(|a, b| a.key_name.cmp(&b.key_name));
    Ok(devices)
}

#[cfg(not(windows))]
pub fn enumerate(_view: RegistryView) -> Result<Vec<J2534Device>, DiscoveryError> {
    Err(DiscoveryError::RegistryUnsupported)
}

/// Enumerates both views. A device installed in both appears once per view.
pub fn enumerate_all() -> Result<Vec<J2534Device>, DiscoveryError> {
    let mut devices = Vec::new();
    for view in RegistryView::ALL {
        devices.extend(enumerate(view)?);
    }
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_is_ok_or_unsupported() {
        match enumerate_all() {
            Ok(_) => {}
            Err(DiscoveryError::RegistryUnsupported) if !cfg!(windows) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}

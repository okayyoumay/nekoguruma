use std::{fmt, io, path::PathBuf};

use tracing::{debug, warn};

#[cfg(windows)]
const REGISTRY_SUBKEY: &str = r"SOFTWARE\PassThruSupport.04.04";
#[cfg(windows)]
const FUNCTION_LIBRARY_VALUE: &str = "FunctionLibrary";

/// Information about an installed J2534 v04.04 device discovered from the Windows registry.
#[derive(Debug, Clone)]
pub struct J2534DeviceInfo {
    /// Registry key name identifying the device (e.g. `"MongoosePro GM II"`).
    pub device_name: String,
    /// Filesystem path to the vendor's PassThru shared library.
    pub library_path: PathBuf,
    /// Architecture of this library entry.
    pub arch: LibraryArch,
}

/// Error type for J2534 registry operations.
#[derive(Debug)]
pub enum RegistryError {
    /// An I/O or registry access error occurred.
    Io(io::Error),
    /// Registry discovery is not available on this platform.
    RegistryUnsupported,
    /// No device matching the requested name was found.
    NotFound(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Io(e) => write!(f, "registry I/O error: {e}"),
            RegistryError::RegistryUnsupported => {
                write!(f, "registry lookup is only supported on Windows")
            }
            RegistryError::NotFound(name) => {
                write!(f, "J2534 device '{name}' not found in registry")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<io::Error> for RegistryError {
    fn from(e: io::Error) -> Self {
        RegistryError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Architecture / view-mode machinery (mirrors iso22900-registry pattern)
// ---------------------------------------------------------------------------

macro_rules! define_arch {
    ($($(#[$attr:meta])* $v:ident$(($s:expr, $f:expr $(,)?))?),* $(,)?) => {
        /// Architecture of a discovered J2534 library.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum LibraryArch {
            /// Matches the current process architecture.
            Native,
            $($(#[$attr])* $v,)*
        }

        /// Controls which registry view is used when querying installed devices.
        ///
        /// On x86_64 Windows, both 32-bit (WOW64) and 64-bit views of the registry
        /// may contain distinct PassThru device entries.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum RegistryViewMode {
            /// Query every architecture-specific view and combine results.
            All,
            /// Query the view that matches the current process architecture.
            Native,
            $($(#[$attr])* $v,)*
        }

        impl From<RegistryViewMode> for LibraryArch {
            fn from(mode: RegistryViewMode) -> Self {
                match mode {
                    $($(#[$attr])* RegistryViewMode::$v => LibraryArch::$v,)*
                    _ => LibraryArch::Native,
                }
            }
        }

        impl From<LibraryArch> for Option<&'static str> {
            fn from(arch: LibraryArch) -> Self {
                match arch {
                    $($(#[$attr])* LibraryArch::$v => [$($s,)? stringify!($v)].first().copied(),)*
                    LibraryArch::Native => None,
                }
            }
        }

        #[cfg(windows)]
        fn get_registry_flags(mode: RegistryViewMode) -> Option<u32> {
            match mode {
                $($(#[$attr])* RegistryViewMode::$v => [$($f)?].first().cloned(),)*
                _ => None,
            }
        }

        fn get_view_mode_list() -> &'static [RegistryViewMode] {
            &[
                $($(#[$attr])* RegistryViewMode::$v,)*
            ]
        }
    }
}

define_arch! {
    #[cfg(windows)] #[cfg(target_arch = "x86_64")] W32("w32", winreg::enums::KEY_WOW64_32KEY),
    #[cfg(windows)] #[cfg(target_arch = "x86_64")] W64("w64", winreg::enums::KEY_WOW64_64KEY),
}

impl LibraryArch {
    /// Returns the `LibraryArch` that matches the current build target.
    #[cfg(windows)]
    #[cfg(target_arch = "x86_64")]
    pub fn get_native() -> LibraryArch {
        LibraryArch::W64
    }
    #[cfg(windows)]
    #[cfg(not(target_arch = "x86_64"))]
    pub fn get_native() -> LibraryArch {
        LibraryArch::Native
    }
    #[cfg(not(windows))]
    pub fn get_native() -> LibraryArch {
        LibraryArch::Native
    }
}

#[cfg(windows)]
#[cfg(target_arch = "x86_64")]
fn get_native_view_mode() -> RegistryViewMode {
    RegistryViewMode::W64
}
#[cfg(windows)]
#[cfg(not(target_arch = "x86_64"))]
fn get_native_view_mode() -> RegistryViewMode {
    RegistryViewMode::Native
}
#[cfg(not(windows))]
fn get_native_view_mode() -> RegistryViewMode {
    RegistryViewMode::Native
}

/// Returns the view list to fold over for `RegistryViewMode::All`: `list`
/// unchanged when non-empty, or `[RegistryViewMode::Native]` when this
/// build enumerates no architecture-specific views at all (see ADR-223).
fn views_for_all(list: &'static [RegistryViewMode]) -> &'static [RegistryViewMode] {
    if list.is_empty() {
        &[RegistryViewMode::Native]
    } else {
        list
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Returns all J2534 v04.04 devices visible under the given registry view.
///
/// - `RegistryViewMode::All` queries every architecture-specific view this
///   build can enumerate and combines the results; devices present in
///   multiple views appear once per view with the corresponding `arch`
///   value. On a build that enumerates no architecture-specific views at
///   all (32-bit Windows, ARM64 Windows, or any non-Windows target), it
///   queries the process's default (`Native`) view instead — see ADR-223.
///   A build that CAN see multiple views (x86_64 Windows) still only sees
///   those views' devices, not a WOW64-detected sibling view; that gap is
///   a documented residual, not covered by `All` here.
/// - `RegistryViewMode::Native` queries the view that matches the current process
///   architecture (64-bit view on x86_64 Windows).
/// - `RegistryViewMode::W32` / `W64` restrict the query to that specific view
///   (only available on x86_64 Windows).
///
/// Devices within each view are sorted by `device_name`.
pub fn enumerate_j2534_devices(
    mode: RegistryViewMode,
) -> Result<Vec<J2534DeviceInfo>, RegistryError> {
    if mode == RegistryViewMode::All {
        views_for_all(get_view_mode_list())
            .iter()
            .try_fold(Vec::new(), |mut acc, &view_mode| {
                match enumerate_j2534_devices(view_mode) {
                    Ok(mut devices) => {
                        acc.append(&mut devices);
                    }
                    Err(RegistryError::NotFound(_)) => {}
                    Err(e) => return Err(e),
                }
                Ok(acc)
            })
    } else {
        let resolved = if mode == RegistryViewMode::Native {
            get_native_view_mode()
        } else {
            mode
        };
        enumerate_devices_impl(resolved)
    }
}

/// Looks up a single J2534 device by its registry key name using the native
/// registry view.
///
/// Returns the first device whose `device_name` matches exactly.
pub fn find_j2534_device(device_name: &str) -> Result<J2534DeviceInfo, RegistryError> {
    find_j2534_device_on_registry(device_name, RegistryViewMode::Native)
}

/// Looks up a single J2534 device by its registry key name using the specified
/// registry view.
pub fn find_j2534_device_on_registry(
    device_name: &str,
    mode: RegistryViewMode,
) -> Result<J2534DeviceInfo, RegistryError> {
    // A device's name is the vendor's and may name the device, and a library path may name a
    // user's directory, so both go in `external`, escaped by `Debug` (ADR-348 item 6).
    debug!(
        ?mode,
        external = ?device_name,
        "looking up J2534 device in registry"
    );
    let result = enumerate_j2534_devices(mode)?
        .into_iter()
        .find(|d| d.device_name == device_name)
        .ok_or_else(|| RegistryError::NotFound(device_name.to_owned()));
    if let Err(RegistryError::NotFound(ref name)) = result {
        warn!(external = ?name, "J2534 device not found in registry");
    }
    result
}

// ---------------------------------------------------------------------------
// Platform implementations
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn enumerate_devices_impl(mode: RegistryViewMode) -> Result<Vec<J2534DeviceInfo>, RegistryError> {
    use winreg::{
        RegKey,
        enums::{HKEY_LOCAL_MACHINE, KEY_READ},
    };

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let flags = KEY_READ | get_registry_flags(mode).unwrap_or(0);

    let root = match hklm.open_subkey_with_flags(REGISTRY_SUBKEY, flags) {
        Ok(key) => key,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(RegistryError::Io(io::Error::other(e.to_string()))),
    };

    let arch: LibraryArch = mode.into();
    debug!(?mode, "enumerating J2534 devices from registry");

    let mut devices: Vec<J2534DeviceInfo> = root
        .enum_keys()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| RegistryError::Io(io::Error::other(e.to_string())))?
        .into_iter()
        .filter_map(|device_name| {
            let key = hklm
                .open_subkey_with_flags(&format!(r"{REGISTRY_SUBKEY}\{device_name}"), flags)
                .ok()?;
            let raw: String = key.get_value(FUNCTION_LIBRARY_VALUE).ok()?;
            let raw = raw.trim().to_owned();
            if raw.is_empty() {
                debug!(
                    external = ?device_name,
                    "skipping J2534 device: FunctionLibrary is empty"
                );
                return None;
            }
            debug!(
                external = ?format!("{device_name}: {raw}"),
                "found J2534 device"
            );
            Some(J2534DeviceInfo {
                device_name,
                library_path: PathBuf::from(raw),
                arch,
            })
        })
        .collect();

    devices.sort_by(|a, b| a.device_name.cmp(&b.device_name));
    debug!(
        count = devices.len(),
        ?mode,
        "J2534 device enumeration complete"
    );
    Ok(devices)
}

#[cfg(not(windows))]
fn enumerate_devices_impl(_mode: RegistryViewMode) -> Result<Vec<J2534DeviceInfo>, RegistryError> {
    Err(RegistryError::RegistryUnsupported)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_native_returns_ok_or_unsupported() {
        match enumerate_j2534_devices(RegistryViewMode::Native) {
            Ok(_) | Err(RegistryError::RegistryUnsupported) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn enumerate_all_returns_ok_or_unsupported() {
        match enumerate_j2534_devices(RegistryViewMode::All) {
            Ok(_) | Err(RegistryError::RegistryUnsupported) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn find_returns_not_found_or_unsupported_for_unknown_device() {
        match find_j2534_device("__nonexistent_j2534_device__") {
            Err(RegistryError::NotFound(_)) | Err(RegistryError::RegistryUnsupported) => {}
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn find_on_registry_all_mode_returns_not_found_or_ok() {
        match find_j2534_device_on_registry("__nonexistent_j2534_device__", RegistryViewMode::All) {
            Err(RegistryError::NotFound(_)) | Err(RegistryError::RegistryUnsupported) => {}
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn views_for_all_falls_back_to_native_when_list_is_empty() {
        assert_eq!(views_for_all(&[]), &[RegistryViewMode::Native]);
    }

    #[test]
    fn views_for_all_returns_the_list_unchanged_when_non_empty() {
        let list: &[RegistryViewMode] = &[RegistryViewMode::Native];
        assert_eq!(views_for_all(list), list);
    }

    #[test]
    fn native_arch_matches_expected() {
        #[cfg(all(windows, target_arch = "x86_64"))]
        assert_eq!(LibraryArch::get_native(), LibraryArch::W64);
        #[cfg(not(all(windows, target_arch = "x86_64")))]
        assert_eq!(LibraryArch::get_native(), LibraryArch::Native);
    }
}

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
    /// Where this entry came from; see [`enumerate_libraries`].
    pub source: LibrarySource,
}

/// Where a [`J2534DeviceInfo`] returned by [`enumerate_libraries`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibrarySource {
    /// Found via Windows registry auto-discovery only.
    Registry,
    /// Present only as a `library_path` entry in the config file (see
    /// [`enumerate_libraries`]); no matching registry entry was found.
    Config,
    /// Found via Windows registry auto-discovery, with the config file
    /// additionally providing a `library_path` override for this device
    /// name — the `library_path` field reflects the configured override.
    Both,
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
    debug!(device_name, ?mode, "looking up J2534 device in registry");
    let result = enumerate_j2534_devices(mode)?
        .into_iter()
        .find(|d| d.device_name == device_name)
        .ok_or_else(|| RegistryError::NotFound(device_name.to_owned()));
    if let Err(RegistryError::NotFound(ref name)) = result {
        warn!(
            device_name = name.as_str(),
            "J2534 device not found in registry"
        );
    }
    result
}

/// Returns a configured `library_path` override for `library_name` from the
/// shared `config.toml` (`config.apis.j2534-0404...`), if present.
///
/// Applies a 2-level priority (highest first):
/// 1. `config.apis.j2534-0404.arch.<arch>.libs.<lib>.library_path` (Windows only)
/// 2. `config.apis.j2534-0404.libs.<lib>.library_path`
///
/// Returns `None` when no override is configured, in which case the caller
/// should fall back to registry auto-discovery (`find_j2534_device`).
pub fn find_library_path(arch: Option<&str>, library_name: &str) -> Option<PathBuf> {
    vci_service_config::find_library_path("j2534-0404", arch, library_name)
}

/// Device names configured with a `library_path` under
/// `config.apis.j2534-0404` in the shared `config.toml`, across both the
/// api-level and every arch-level `libs` table. Sorted and deduplicated.
pub fn list_configured_libraries() -> Vec<String> {
    vci_service_config::list_configured_libraries("j2534-0404")
}

/// Enumerates all J2534 v04.04 devices visible to this process: registry
/// auto-discovery ([`enumerate_j2534_devices`]) merged with `library_path`
/// entries configured under `config.apis.j2534-0404` in the shared
/// `config.toml` ([`list_configured_libraries`], including its `arch`-level
/// entries).
///
/// A device name present in both sources appears once, with `library_path`
/// set to the configured override (matching the priority
/// [`resolve_library_path`] applies at startup) and `source` set to
/// [`LibrarySource::Both`]. A name present in only one source keeps that
/// source's fields, with `source` set to [`LibrarySource::Registry`] or
/// [`LibrarySource::Config`] accordingly.
///
/// Registry auto-discovery failures (e.g. `RegistryUnsupported` on
/// non-Windows) are treated as "no registry-discovered devices" rather than
/// propagated, so a config-only device is always listed regardless of
/// platform.
///
/// Sorted by `device_name`.
pub fn enumerate_libraries(mode: RegistryViewMode) -> Vec<J2534DeviceInfo> {
    let mut devices = match enumerate_j2534_devices(mode) {
        Ok(devices) => devices,
        Err(RegistryError::RegistryUnsupported) => {
            debug!(
                "registry auto-discovery unsupported on this platform; falling back to config-only libraries"
            );
            Vec::new()
        }
        Err(e) => {
            warn!(error = %e, "registry auto-discovery failed while enumerating libraries; falling back to config-only libraries");
            Vec::new()
        }
    };

    for (name, path) in vci_service_config::list_configured_library_paths("j2534-0404") {
        if let Some(existing) = devices.iter_mut().find(|d| d.device_name == name) {
            existing.library_path = path;
            existing.source = LibrarySource::Both;
        } else {
            devices.push(J2534DeviceInfo {
                device_name: name,
                library_path: path,
                arch: LibraryArch::get_native(),
                source: LibrarySource::Config,
            });
        }
    }

    devices.sort_by(|a, b| a.device_name.cmp(&b.device_name));
    devices
}

/// Resolves the filesystem path of the J2534 v04.04 library named
/// `library_name`, applying the same priority `J2534Service::new()` used to
/// apply itself: a configured `library_path` override (`find_library_path`)
/// takes priority when present, falling back to registry auto-discovery
/// (`find_j2534_device`) otherwise.
pub fn resolve_library_path(
    arch: Option<&str>,
    library_name: &str,
) -> Result<PathBuf, RegistryError> {
    if let Some(path) = find_library_path(arch, library_name) {
        return Ok(path);
    }
    Ok(find_j2534_device(library_name)?.library_path)
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
                    device_name,
                    "skipping J2534 device: FunctionLibrary is empty"
                );
                return None;
            }
            debug!(device_name, library_path = raw, "found J2534 device");
            Some(J2534DeviceInfo {
                device_name,
                library_path: PathBuf::from(raw),
                arch,
                source: LibrarySource::Registry,
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

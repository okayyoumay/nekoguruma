//! Config file loading for VCI gRPC services: locates and parses
//! `config.toml` and exposes priority-based lookups for logging settings
//! (`find_logging_config`), library path overrides (`find_library_path`),
//! vendor STRUCTFIELD entry sizes (`find_vendor_struct_type_size`, plus its
//! startup key-grammar check `validate_vendor_struct_type_keys`, see
//! ADR-218), and `vci-service-manager` runtime settings (`manager_config`,
//! see ADR-073).
//!
//! Extracted from `vci-service-launcher` (see ADR-033) so it can be reused
//! by consumers, such as `vci-service-manager`, that do not want the rest of
//! `vci-service-launcher`'s gRPC/JSON-RPC bootstrap machinery.

use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

/// Sanity ceiling on a configured `vendor_struct_types` entry size (bytes),
/// enforced in two places (ADR-218, as amended — Codex review, PR #133
/// twelfth and thirteenth rounds):
///
/// 1. [`validate_vendor_struct_type_table_keys`], once at startup -- fails
///    loudly, naming the offending key, so an operator typo is caught
///    immediately rather than discovered at request time.
/// 2. [`find_vendor_struct_type_size`], on every lookup -- fails safe
///    (treats an oversized value the same as "not configured at this
///    level"), since that function re-reads `config.toml` fresh on every
///    call and a value edited into the file *after* startup would
///    otherwise bypass check 1 entirely and reach a caller unbounded.
///
/// Mirrors `j2534-0404-service::config::VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES`'s
/// identical role for the sibling `vendor_ioctls` table, with this second
/// layer added because that table's eager-loaded-once-at-startup design
/// (see its own doc comment) has no equivalent post-startup exposure window
/// to close: this is a **rejection ceiling**, not an allocation cap
/// substituted for the configured value -- a value within it is honored
/// exactly as configured; only a value above it is ever refused. Without
/// this, an operator typo (e.g. `0xffffffff`) reaches
/// `BorrowedVendorSpecificStructArray::bytes` unchecked at request time,
/// where it is multiplied by the native entry count to build a byte length
/// for `ffi_slice`/`.to_vec()` -- a native result with even a single entry
/// then attempts a multi-gigabyte allocation, aborting the service (or,
/// for a smaller-but-still-oversized value, reads past the DLL-owned
/// allocation -- undefined behavior, not merely an abort).
///
/// 64 KiB is generous relative to any plausible real vendor STRUCTFIELD
/// entry: every SAE/ISO-defined ComParam struct in this workspace
/// (`PDU_PARAM_STRUCT_SESS_TIMING`/`ACCESS_TIMING`/
/// `TLS_VERSION_AND_CIPHER`) is well under 64 bytes, and a vendor-specific
/// struct is still a single fixed-layout native record, not a bulk transfer
/// like a raw vendor IOCTL's buffer -- so this ceiling is far smaller than
/// `VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES`'s 16 MiB by design, not by
/// oversight. Raising it is a one-line, reviewable change if a real vendor
/// struct ever legitimately needs more.
pub(crate) const VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES: u32 = 64 * 1024;

/// Error loading `config.toml` from disk: either the file exists but could
/// not be read, or it was read but could not be parsed as valid TOML
/// matching the expected schema.
///
/// Most lookups in this module (e.g. [`find_logging_config`],
/// [`find_library_path`]) intentionally swallow this error and fall back to
/// defaults — see [`load_toml_config`]'s doc comment. [`find_modules`] is the
/// one exception: it surfaces this error to the caller instead, since a
/// malformed config file makes the real "is there a modules list configured"
/// answer unknowable (see ADR-107 addendum (j)).
#[derive(Debug)]
pub enum ConfigFileError {
    /// The config file exists but could not be read (permissions, I/O
    /// error, etc.) — anything other than the file simply not existing.
    Read { path: PathBuf, source: io::Error },
    /// The config file was read but its contents are not valid TOML, or do
    /// not match the expected schema.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

impl std::fmt::Display for ConfigFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigFileError::Read { path, source } => {
                write!(f, "failed to read config file {}: {source}", path.display())
            }
            ConfigFileError::Parse { path, source } => {
                write!(
                    f,
                    "failed to parse config file {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ConfigFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigFileError::Read { source, .. } => Some(source),
            ConfigFileError::Parse { source, .. } => Some(source),
        }
    }
}

/// Logging settings that can appear at any level in the config hierarchy.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct LoggingConfig {
    /// `tracing-subscriber` EnvFilter directive string.
    /// Examples: `"info"`, `"debug"`, `"j2534_0404=debug,info"`.
    pub level: Option<String>,
    /// Log output destination.  Defaults to `stderr` when omitted.
    pub output: Option<LogOutput>,
}

/// Log output destination.
///
/// TOML examples:
/// ```toml
/// output = "stderr"
/// output = "null"
/// output = { file = "C:/ProgramData/nekoguruma/logs/service.log" }
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum LogOutput {
    /// Named destination: `"stderr"` or `"null"`.
    Named(LogOutputKind),
    /// Write to a file.
    File(FileOutput),
}

impl Default for LogOutput {
    fn default() -> Self {
        LogOutput::Named(LogOutputKind::Stderr)
    }
}

/// Named log output destinations.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LogOutputKind {
    /// Write to standard error (default).
    /// Standard output must not be used because it is reserved for
    /// JSON-RPC communication with the parent process.
    Stderr,
    /// Discard all log output.
    Null,
    /// Write to the Windows Event Log.
    /// Requires the `eventlog` Cargo feature; falls back to stderr on other platforms.
    #[cfg(feature = "eventlog")]
    Eventlog,
    /// Write to the systemd journal (journald).
    /// Requires the `journald` Cargo feature; falls back to stderr on other platforms.
    #[cfg(feature = "journald")]
    Journald,
}

/// Configuration for file-based log output.
#[derive(Debug, Clone, Deserialize)]
pub struct FileOutput {
    /// Absolute path to the log file.  The file is created if it does not
    /// exist and new entries are always appended.
    pub file: String,
}

/// One pre-declared J2534 v04.04 device-selection entry, from a
/// `[[config.apis.j2534-0404.libs."<lib>".modules]]` array-of-tables entry
/// (see ADR-106).
///
/// J2534-1 v04.04's `PassThruOpen` has no in-spec way to select which
/// physical device to open (`pName` must be NULL per spec). Each configured
/// entry pre-declares a connection target: `pname` is passed as
/// `PassThruOpen`'s non-NULL `pName` argument (an out-of-spec
/// vendor-extension some J2534 DLLs support), and `label` is a
/// human-readable name surfaced to gRPC clients via `GetModuleIds`
/// (`ModuleData.vendor_module_name`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModuleConfigEntry {
    /// Human-readable name for this device, surfaced as
    /// `ModuleData.vendor_module_name`.
    pub label: String,
    /// Connection-target string passed verbatim as `PassThruOpen`'s `pName`
    /// argument. Must be ASCII with no embedded NUL byte — validated by the
    /// consuming service (`j2534-0404-service`) at startup, not here.
    pub pname: String,
}

/// One operator-declared vendor IOCTL command's native contract, from a
/// `[config.apis.j2534-0404.libs."<lib>".vendor_ioctls."0x<cmd_id>"]` table
/// (ADR-219, as amended). `j2534-0404-service`'s vendor IOCTL passthrough
/// (`cmd_id >= 0x10000`) has no other way to learn a vendor command's real
/// native buffer shape/sizes — the D-PDU/J2534 API surfaces no self-describing
/// metadata for a tool-manufacturer-reserved IOCTL, so this config table is
/// the sole source of truth, mirroring ADR-218's identical
/// `vendor_struct_types` precedent (`VendorIoctlConfigEntry` is this ADR's
/// counterpart to that ADR's per-`ComParamStructType` entry size).
///
/// `#[serde(deny_unknown_fields)]` (Codex review, PR #133 eighth round): a
/// misspelled safety-critical field (`input_requred` for `input_required`,
/// `input_byte` for `input_bytes`) would otherwise deserialize silently --
/// serde simply ignores an unrecognized key, so the misspelled field's
/// `#[serde(default)]` counterpart (`false`/`0`) takes over unnoticed,
/// letting a contract that was meant to require a buffer instead permit a
/// NULL one. Rejecting an unknown key at startup surfaces the typo
/// immediately, the same "fail fast, don't silently no-op" convention this
/// table's key-grammar and shape-mismatch checks already apply.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VendorIoctlConfigEntry {
    /// `"raw"` (direct `pInput`/`pOutput` pointers) or `"sbyte_array"`
    /// (each pointer is a native `SBYTE_ARRAY`) — validated by the consuming
    /// service (`j2534-0404-service`), not here, the same division of
    /// responsibility [`ModuleConfigEntry::pname`]'s doc comment describes
    /// for its own domain-specific validation.
    pub shape: String,
    /// Maximum bytes the vendor DLL reads from `pInput` in raw mode. Only
    /// meaningful when `shape == "raw"`; `0` means that direction takes no
    /// input buffer. Ignored for `shape == "sbyte_array"` (wrapped mode's
    /// own `SBYTE_ARRAY.NumOfBytes` mechanism self-describes the real
    /// length instead).
    #[serde(default)]
    pub input_bytes: u32,
    /// Maximum bytes the vendor DLL writes to `pOutput` in raw mode. Only
    /// meaningful when `shape == "raw"`; `0` means no output buffer.
    /// Ignored for `shape == "sbyte_array"`, for the same reason as
    /// [`Self::input_bytes`].
    #[serde(default)]
    pub output_bytes: u32,
    /// Whether this command's native contract always reads a non-NULL
    /// `SBYTE_ARRAY` through `pInput`. Only meaningful when
    /// `shape == "sbyte_array"` (`shape == "raw"` already expresses the
    /// identical requirement via a nonzero [`Self::input_bytes`] instead,
    /// so setting this alongside `shape == "raw"` is a startup error). A
    /// present-but-empty wrapped input (`Some(vec![])`, `NumOfBytes: 0`) is
    /// enough to satisfy this -- unlike raw mode's `input_bytes`, this is a
    /// pointer-presence requirement, not a byte-count one; whether an empty
    /// array is otherwise acceptable is the vendor DLL's own business
    /// (ADR-219, as amended).
    #[serde(default)]
    pub input_required: bool,
    /// Whether this command's native contract always writes a non-NULL
    /// `SBYTE_ARRAY` through `pOutput`. Only meaningful when
    /// `shape == "sbyte_array"`, for the same reason as
    /// [`Self::input_required`].
    #[serde(default)]
    pub output_required: bool,
}

/// Settings for `vci-service-manager`, from the `[config.manager]` table
/// (see ADR-073).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ManagerConfig {
    /// TCP bind address, e.g. `"127.0.0.1:8080"`. Defaults to
    /// `"127.0.0.1:8080"` when unset; the default is applied by the manager,
    /// not by this struct.
    pub bind: Option<String>,
    /// URL path prefix under which all endpoints are served, e.g. `"/vci"`.
    /// Defaults to no prefix when unset.
    pub root_path: Option<String>,
    /// Local IPC endpoint the manager listens on for peer-identity-authorized
    /// clients (see ADR-226): a filesystem path to a Unix domain socket on
    /// Linux/macOS, or a Windows named pipe name. Defaults are applied by the
    /// manager, not by this struct: `$XDG_RUNTIME_DIR/vci-service-manager.sock`
    /// (falling back to `/tmp/vci-service-manager-<uid>.sock`) on Linux/macOS,
    /// `\\.\pipe\vci-service-manager` on Windows.
    pub ipc: Option<String>,
}

// ── Internal TOML schema ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct InstanceConfig {
    logging: Option<LoggingConfig>,
    /// Explicit filesystem path to this library, overriding platform
    /// auto-discovery (e.g. the Windows registry) when present.
    library_path: Option<String>,
    /// CAN-bus channel operating mode for this library. Consumed by J2534
    /// adapter services (`j2534-0404-service`); the value is an opaque string
    /// here — validation happens in the consuming service.
    can_channel_mode: Option<String>,
    /// Pre-declared J2534 v04.04 device-selection entries for this library
    /// (ADR-106). Consumed by `j2534-0404-service`; validation (non-empty,
    /// ASCII `pname` with no embedded NUL) happens in the consuming service.
    modules: Option<Vec<ModuleConfigEntry>>,
    /// Per-`ComParamStructType` vendor STRUCTFIELD entry sizes (bytes) for
    /// this library (ADR-218). Consumed by `iso22900-service`'s
    /// `find_vendor_struct_type_size`. Each key is the same
    /// `"0x<8 lowercase hex digits>"` string used in the gRPC
    /// `ParamVendorSpecificStruct.type_url` suffix.
    vendor_struct_types: Option<HashMap<String, u32>>,
    /// Per-`cmd_id` vendor IOCTL native contracts (shape + buffer sizes) for
    /// this library (ADR-219, as amended). Consumed by
    /// `j2534-0404-service`'s `find_vendor_ioctl_contract`. Each key is the
    /// same `"0x<8 lowercase hex digits>"` grammar `vendor_struct_types`
    /// uses, formatted from the `IoctlID`/`cmd_id`.
    vendor_ioctls: Option<HashMap<String, VendorIoctlConfigEntry>>,
}

#[derive(Debug, Deserialize, Default)]
struct ArchConfig {
    logging: Option<LoggingConfig>,
    libs: Option<HashMap<String, InstanceConfig>>,
}

#[derive(Debug, Deserialize, Default)]
struct ApiConfig {
    logging: Option<LoggingConfig>,
    arch: Option<HashMap<String, ArchConfig>>,
    libs: Option<HashMap<String, InstanceConfig>>,
    /// API-level default CAN-bus channel operating mode, applied when no
    /// per-library `can_channel_mode` matches.
    can_channel_mode: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RootConfig {
    logging: Option<LoggingConfig>,
    apis: Option<HashMap<String, ApiConfig>>,
    manager: Option<ManagerConfig>,
}

/// Top-level TOML document: `[config]` table.
#[derive(Debug, Deserialize, Default)]
struct TomlConfig {
    config: Option<RootConfig>,
}

// ── File location ─────────────────────────────────────────────────────────────

fn config_file_path() -> PathBuf {
    // Path to the config file: either relative to the selected root, or an
    // absolute path that bypasses root resolution entirely. Defaults to the
    // build-time value embedded via VCI_CONFIG_PATH (default:
    // "nekoguruma/config.toml", so `/etc/nekoguruma/config.toml` on Linux,
    // ADR-228). In debug builds only
    // (`debug_assertions`, see ADR-073), a runtime VCI_CONFIG_PATH
    // environment variable takes precedence when set, letting IDE debug
    // launch configurations (see .vscode/launch.json) point at a dedicated
    // config file without a rebuild. Release builds read no environment
    // variable here and use solely the build-time embedded value.
    const CONFIG_PATH: &str = env!("VCI_CONFIG_PATH");
    #[cfg(debug_assertions)]
    let path = runtime_override(std::env::var_os("VCI_CONFIG_PATH"))
        .unwrap_or_else(|| PathBuf::from(CONFIG_PATH));
    #[cfg(not(debug_assertions))]
    let path = PathBuf::from(CONFIG_PATH);
    resolve_config_path(&path)
}

/// Directory of the J2534 registration definitions on Linux (design 7.1.1): one file per VCI,
/// written by the administrator. Fixed at build time (ADR-228): `NGR_J2534_DEFINITION_DIR` at
/// build time, either absolute or relative to the fixed system directory of
/// [`fixed_system_root`] (default `nekoguruma/j2534`, so `/etc/nekoguruma/j2534` on Linux).
/// Unlike the configuration file, no `config-root-*` feature moves it: definitions are read
/// from the administrator's directory in every build and mode (ADR-228 Decision 2). In debug
/// builds only (ADR-073), a non-empty runtime `NGR_J2534_DEFINITION_DIR` takes precedence, so
/// tests can point it at a directory of their own; release builds read no environment variable
/// here.
pub fn j2534_definition_dir() -> PathBuf {
    const DEFINITION_DIR: &str = env!("NGR_J2534_DEFINITION_DIR");
    #[cfg(debug_assertions)]
    let path = runtime_override(std::env::var_os("NGR_J2534_DEFINITION_DIR"))
        .unwrap_or_else(|| PathBuf::from(DEFINITION_DIR));
    #[cfg(not(debug_assertions))]
    let path = PathBuf::from(DEFINITION_DIR);
    if path.is_absolute() {
        path
    } else {
        fixed_system_root().join(path)
    }
}

/// A debug-only runtime override (ADR-073) from an environment variable's value: none when
/// the variable is unset or empty, so an empty value falls back to the build-time location
/// instead of resolving to the root itself.
#[cfg(debug_assertions)]
fn runtime_override(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// The administrator-only system configuration directory of the platform, whatever
/// `config-root-*` features the build has: `%ProgramData%` on Windows, `/private/etc` on macOS
/// (`/etc` is a symlink there), `/etc` on every other Unix.
fn fixed_system_root() -> PathBuf {
    #[cfg(windows)]
    {
        known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_ProgramData)
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/private/etc")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        PathBuf::from("/etc")
    }
}

/// Resolves `config_path` against the selected root, unless `config_path` is
/// itself absolute, in which case it is returned as-is and `config_root()`
/// is never invoked (avoiding e.g. an unnecessary `SHGetKnownFolderPath`
/// call, or a panic from `current_exe()`, when the caller only wants a fixed
/// absolute file).
fn resolve_config_path(config_path: &Path) -> PathBuf {
    if config_path.is_absolute() {
        return config_path.to_path_buf();
    }
    config_root().join(config_path)
}

// ── Root directory selection ──────────────────────────────────────────────────
//
// Exactly one of the three `config_root_impl()` functions below is compiled
// for any given (target, feature) combination.

/// Returns the config file root directory.
///
/// In test builds, checks a `#[cfg(test)]`-only thread-local override first
/// (see `test_support` below) so unit tests never touch the real exe
/// directory, a real Windows known folder, or `/etc`. In non-test builds this
/// call is optimized away entirely (the whole `#[cfg(test)]` block does not
/// exist), so production behavior and codegen are unchanged.
fn config_root() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = test_support::current_override() {
        return p;
    }
    config_root_impl()
}

/// `config-root-exe-dir` (all platforms): directory containing the executable.
#[cfg(feature = "config-root-exe-dir")]
fn config_root_impl() -> PathBuf {
    let exe = std::env::current_exe().expect("cannot resolve executable path");
    exe.parent()
        .expect("executable has no parent directory")
        .to_path_buf()
}

/// Windows without `config-root-exe-dir`: Windows known folder resolved at
/// runtime via `SHGetKnownFolderPath`; which `FOLDERID_*` to request is
/// selected at build time by the active `config-root-win-*` feature
/// (default: `FOLDERID_ProgramData`).
#[cfg(all(windows, not(feature = "config-root-exe-dir")))]
fn config_root_impl() -> PathBuf {
    windows_known_folder_root()
}

/// Non-Windows without `config-root-exe-dir`: `/etc`, writable by the administrator only
/// (ADR-228).
#[cfg(all(not(windows), not(feature = "config-root-exe-dir")))]
fn config_root_impl() -> PathBuf {
    PathBuf::from("/etc")
}

// ── Windows known-folder helper ───────────────────────────────────────────────

#[cfg(all(windows, not(feature = "config-root-exe-dir")))]
fn windows_known_folder_root() -> PathBuf {
    // Select the FOLDERID at compile time based on Cargo features.
    // If more than one config-root-win-* feature is enabled simultaneously,
    // the first match in this priority chain takes effect. Each FOLDERID_*
    // import is gated identically to its `let rfid = ...` below, so exactly
    // one is ever imported (and none are unused) for a given feature set.
    #[cfg(feature = "config-root-win-local-app-data")]
    use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;
    #[cfg(feature = "config-root-win-local-app-data")]
    let rfid = &FOLDERID_LocalAppData;
    #[cfg(all(
        not(feature = "config-root-win-local-app-data"),
        feature = "config-root-win-roaming-app-data",
    ))]
    use windows_sys::Win32::UI::Shell::FOLDERID_RoamingAppData;
    #[cfg(all(
        not(feature = "config-root-win-local-app-data"),
        feature = "config-root-win-roaming-app-data",
    ))]
    let rfid = &FOLDERID_RoamingAppData;
    #[cfg(all(
        not(feature = "config-root-win-local-app-data"),
        not(feature = "config-root-win-roaming-app-data"),
        feature = "config-root-win-program-files",
    ))]
    use windows_sys::Win32::UI::Shell::FOLDERID_ProgramFiles;
    #[cfg(all(
        not(feature = "config-root-win-local-app-data"),
        not(feature = "config-root-win-roaming-app-data"),
        feature = "config-root-win-program-files",
    ))]
    let rfid = &FOLDERID_ProgramFiles;
    #[cfg(all(
        not(feature = "config-root-win-local-app-data"),
        not(feature = "config-root-win-roaming-app-data"),
        not(feature = "config-root-win-program-files"),
        feature = "config-root-win-program-files-x86",
    ))]
    use windows_sys::Win32::UI::Shell::FOLDERID_ProgramFilesX86;
    #[cfg(all(
        not(feature = "config-root-win-local-app-data"),
        not(feature = "config-root-win-roaming-app-data"),
        not(feature = "config-root-win-program-files"),
        feature = "config-root-win-program-files-x86",
    ))]
    let rfid = &FOLDERID_ProgramFilesX86;
    // Default when no config-root-win-* feature is active.
    #[cfg(not(any(
        feature = "config-root-win-local-app-data",
        feature = "config-root-win-roaming-app-data",
        feature = "config-root-win-program-files",
        feature = "config-root-win-program-files-x86",
    )))]
    use windows_sys::Win32::UI::Shell::FOLDERID_ProgramData;
    #[cfg(not(any(
        feature = "config-root-win-local-app-data",
        feature = "config-root-win-roaming-app-data",
        feature = "config-root-win-program-files",
        feature = "config-root-win-program-files-x86",
    )))]
    let rfid = &FOLDERID_ProgramData;

    known_folder(rfid)
}

/// Resolves a Windows known folder via `SHGetKnownFolderPath`, given an
/// arbitrary `FOLDERID_*` GUID. Extracted out of `windows_known_folder_root`
/// (ADR-226 SS3 amendment) so [`system_config_dir`] below can call it
/// unconditionally with a FIXED `FOLDERID_ProgramData`, independent of
/// whichever `config-root-win-*`/`config-root-exe-dir` feature happens to be
/// active -- unlike `windows_known_folder_root()` itself, which is gated
/// behind `not(feature = "config-root-exe-dir")` and picks its FOLDERID via
/// that feature-priority chain. Not itself gated behind any
/// `config-root-*` feature: it compiles, and is callable, on every Windows
/// build regardless of which of those features is active.
#[cfg(windows)]
fn known_folder(rfid: &windows_sys::core::GUID) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;

    let mut path_ptr: *mut u16 = std::ptr::null_mut();
    // SAFETY: `rfid` is a reference to a valid GUID constant; `path_ptr` is
    // initialised to null and will be populated by the API; passing 0 for the
    // access token requests the current-user context.
    let hr = unsafe { SHGetKnownFolderPath(rfid, 0, 0, &mut path_ptr) };
    if hr != 0 {
        panic!("SHGetKnownFolderPath failed: HRESULT {hr:#010x}");
    }

    // SAFETY: S_OK (0) was returned, so `path_ptr` is a valid null-terminated
    // wide string allocated by the Shell; it must be freed with `CoTaskMemFree`.
    let len = unsafe {
        let mut n = 0usize;
        while *path_ptr.add(n) != 0 {
            n += 1;
        }
        n
    };
    let wide = unsafe { std::slice::from_raw_parts(path_ptr, len) };
    let root = PathBuf::from(OsString::from_wide(wide));
    unsafe { CoTaskMemFree(path_ptr as *const ::core::ffi::c_void) };

    root
}

/// Reads and parses the config file, returning `Err` when the file exists
/// but cannot be read or parsed. A missing file is not an error — it is a
/// legitimate "nothing configured" state, distinct from "file exists but is
/// broken" — and resolves to `Ok(TomlConfig::default())`.
fn try_load_toml_config() -> Result<TomlConfig, ConfigFileError> {
    let path = config_file_path();
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(TomlConfig::default()),
        Err(e) => return Err(ConfigFileError::Read { path, source: e }),
    };
    toml::from_str::<TomlConfig>(&content).map_err(|e| ConfigFileError::Parse { path, source: e })
}

/// Lenient wrapper around [`try_load_toml_config`]: a read or parse failure
/// is logged to stderr and treated as "nothing configured"
/// (`TomlConfig::default()`), rather than propagated. Used by every lookup
/// in this module except [`find_modules`], which cannot safely treat a
/// malformed document as "absent" (see [`ConfigFileError`]'s doc comment).
fn load_toml_config() -> TomlConfig {
    try_load_toml_config().unwrap_or_else(|e| {
        eprintln!("warning: {e}");
        TomlConfig::default()
    })
}

// ── Priority-based lookup ─────────────────────────────────────────────────────

/// Return the logging config for the given service instance, applying the
/// priority hierarchy (highest first):
///
/// 1. `config.apis.<api>.arch.<arch>.libs.<lib>` (Windows only)
/// 2. `config.apis.<api>.arch.<arch>`             (Windows only)
/// 3. `config.apis.<api>.libs.<lib>`
/// 4. `config.apis.<api>`
/// 5. `config`
pub fn find_logging_config(
    api_name: &str,
    arch: Option<&str>,
    library_name: &str,
) -> LoggingConfig {
    let toml = load_toml_config();
    let root = match toml.config {
        Some(r) => r,
        None => return LoggingConfig::default(),
    };

    // Priorities 1 & 2 — arch-specific (Windows only)
    if let (Some(arch_key), Some(apis)) = (arch, &root.apis)
        && let Some(api_cfg) = apis.get(api_name)
        && let Some(ref arch_map) = api_cfg.arch
        && let Some(arch_cfg) = arch_map.get(arch_key)
    {
        // Priority 1: arch + library
        if let Some(ref libs) = arch_cfg.libs
            && let Some(inst) = libs.get(library_name)
            && let Some(logging) = inst.logging.clone()
        {
            return logging;
        }
        // Priority 2: arch only
        if let Some(logging) = arch_cfg.logging.clone() {
            return logging;
        }
    }

    if let Some(ref apis) = root.apis
        && let Some(api_cfg) = apis.get(api_name)
    {
        // Priority 3: api + library
        if let Some(ref libs) = api_cfg.libs
            && let Some(inst) = libs.get(library_name)
            && let Some(logging) = inst.logging.clone()
        {
            return logging;
        }
        // Priority 4: api
        if let Some(logging) = api_cfg.logging.clone() {
            return logging;
        }
    }

    // Priority 5: root config
    root.logging.unwrap_or_default()
}

/// Return a configured `library_path` override for the given service
/// instance, if one is present in the config file.
///
/// Applies the following priority hierarchy (highest first):
///
/// 1. `config.apis.<api>.arch.<arch>.libs.<lib>.library_path` (Windows only)
/// 2. `config.apis.<api>.libs.<lib>.library_path`
///
/// Unlike [`find_logging_config`], there is no api-level or root-level
/// fallback: a `library_path` only makes sense tied to a specific library
/// name, so entries without a `libs.<lib>` key are never consulted.
///
/// Returns `None` when no override is configured, in which case the caller
/// should fall back to platform-specific auto-discovery (e.g. the Windows
/// registry).
pub fn find_library_path(
    api_name: &str,
    arch: Option<&str>,
    library_name: &str,
) -> Option<PathBuf> {
    let toml = load_toml_config();
    let api_cfg = toml.config?.apis?.remove(api_name)?;

    // Priority 1 — arch + library (Windows only)
    if let (Some(arch_key), Some(mut arch_map)) = (arch, api_cfg.arch)
        && let Some(path) = arch_map
            .remove(arch_key)
            .and_then(|arch_cfg| arch_cfg.libs)
            .and_then(|mut libs| libs.remove(library_name))
            .and_then(|inst| inst.library_path)
    {
        return Some(PathBuf::from(path));
    }

    // Priority 2 — api + library
    api_cfg
        .libs?
        .remove(library_name)?
        .library_path
        .map(PathBuf::from)
}

/// Return a configured vendor STRUCTFIELD entry size (bytes) for the given
/// `struct_type`, if one is present in the config file (ADR-218).
///
/// Applies the same two-level priority hierarchy as [`find_library_path`]
/// (highest first):
///
/// 1. `config.apis.<api>.arch.<arch>.libs.<lib>.vendor_struct_types."<key>"` (Windows only)
/// 2. `config.apis.<api>.libs.<lib>.vendor_struct_types."<key>"`
///
/// `<key>` is `struct_type` formatted as `"0x<8 lowercase hex digits>"`,
/// matching the grammar used by the gRPC `ParamVendorSpecificStruct.type_url`
/// suffix (ADR-218 Decision item 3). Like [`find_library_path`], there is no
/// api-level or root-level fallback: a vendor struct-field entry size only
/// makes sense tied to a specific library.
///
/// Returns `None` when no matching entry is configured, in which case the
/// caller (`iso22900-service`) fails `FAILED_PRECONDITION` -- this config
/// table is the sole source of a vendor struct type's entry size, for both
/// `GetComParam`/`GetUniqueRespIdTable` reads and `SetComParam`/
/// `SetUniqueRespIdTable` writes (ADR-218 Decision item 4, as amended to
/// remove a process-lifetime write cache found to be a local
/// heap-disclosure vector).
///
/// A returned size is always at or below
/// [`VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES`] -- this function re-checks the
/// ceiling itself at lookup time (not only relying on
/// [`validate_vendor_struct_type_keys`]'s startup check), since it re-reads
/// `config.toml` from disk fresh on every call and a value edited into the
/// file after startup would otherwise reach a caller unbounded (ADR-218, as
/// amended; Codex review, PR #133 thirteenth round -- the initial fix only
/// checked at startup, the same limited scope as the key-grammar check,
/// but unlike a noncanonical key an oversized size does NOT fail safe if it
/// reaches a caller, so a startup-only check was insufficient here). An
/// entry above the ceiling is treated as unusable at that priority level,
/// the same treatment already given to a `0` below.
pub fn find_vendor_struct_type_size(
    api_name: &str,
    arch: Option<&str>,
    library_name: &str,
    struct_type: u32,
) -> Option<u32> {
    let key = format!("0x{struct_type:08x}");
    let toml = load_toml_config();
    let api_cfg = toml.config?.apis?.remove(api_name)?;

    // Priority 1 — arch + library (Windows only). A resolved `0` is treated
    // the same as "not configured at this level" (matching
    // `resolve_vendor_struct_entry_size`'s own `size != 0` filter) so a
    // zero placeholder at the arch level cannot shadow a real nonzero value
    // configured at the api+library level (Codex review, PR #133 sixth
    // round): without this filter, an arch-level `0` returned early here,
    // permanently failing every nonempty read/write for this struct type
    // even though a usable fallback was configured one level down. An
    // oversized size (above VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES) gets
    // the identical "not configured at this level" treatment, falling
    // through to the api+library level rather than ever being returned
    // (Codex review, PR #133 thirteenth round).
    if let (Some(arch_key), Some(mut arch_map)) = (arch, api_cfg.arch)
        && let Some(size) = arch_map
            .remove(arch_key)
            .and_then(|arch_cfg| arch_cfg.libs)
            .and_then(|mut libs| libs.remove(library_name))
            .and_then(|inst| inst.vendor_struct_types)
            .and_then(|mut table| table.remove(&key))
        && size != 0
        && size <= VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES
    {
        return Some(size);
    }

    // Priority 2 — api + library. An oversized size is filtered out the
    // same way the arch level's is above (Codex review, PR #133 thirteenth
    // round) -- there is no further level to fall through to, so this
    // resolves to `None`, the same "unconfigured" outcome an absent entry
    // already produces.
    api_cfg
        .libs?
        .remove(library_name)?
        .vendor_struct_types?
        .remove(&key)
        .filter(|&size| size <= VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES)
}

/// Validates that every key in the configured library's `vendor_struct_types`
/// table (if any) matches the canonical `"0x<8 lowercase hex digits>"`
/// grammar (ADR-218 Decision item 3) -- the same grammar
/// [`find_vendor_struct_type_size`] always constructs for its own lookup key.
///
/// [`find_vendor_struct_type_size`] resolves a key independently at each
/// priority level (a key present in only the arch+lib table, or only the
/// api+lib table, is still reachable there -- see its doc comment), unlike
/// [`find_vendor_ioctls`]'s whole-table-wins precedent. This validates both
/// levels' tables independently for the same reason: a noncanonical key
/// (wrong case, wrong width) in *either* table would otherwise never match
/// the canonical key [`find_vendor_struct_type_size`] constructs from a
/// `struct_type` at request time, silently making that entry unreachable
/// while `config.toml` looks correctly configured.
///
/// Like [`find_modules`] (not the lenient [`load_toml_config`] every other
/// lookup in this module uses), a config file that exists but cannot be read
/// or parsed is propagated as `Err` here rather than treated as "nothing to
/// validate": `find_library_path`'s own lenient load means a broken config
/// file does NOT reliably fail startup elsewhere on its own, since
/// `resolve_library_path` can fall back to platform auto-discovery (e.g.
/// RDF) when it sees `None` for exactly the same reason a broken document
/// would produce `None` here -- so treating a broken file as "nothing
/// configured" would let a malformed `vendor_struct_types` table (unreadable
/// due to the very same broken document) pass validation silently while
/// service startup proceeds, defeating this function's purpose the same way
/// [`find_modules`]'s own doc comment describes for `modules` (Codex review,
/// PR #133 sixth round).
///
/// Returns `Err` naming the first offending key found (arch+lib table first,
/// if `arch` is `Some` and that table has one, then the api+lib table), or
/// the config file's own read/parse error if it can't be loaded at all.
/// Returns `Ok(())` when the file loads cleanly and no `vendor_struct_types`
/// table is configured for this library at all -- nothing to validate.
pub fn validate_vendor_struct_type_keys(
    api_name: &str,
    arch: Option<&str>,
    library_name: &str,
) -> Result<(), String> {
    let toml = try_load_toml_config().map_err(|e| e.to_string())?;
    let Some(api_cfg) = toml
        .config
        .and_then(|root| root.apis)
        .and_then(|mut apis| apis.remove(api_name))
    else {
        return Ok(());
    };

    // Priority 1 — arch + library (Windows only)
    if let (Some(arch_key), Some(arch_map)) = (arch, &api_cfg.arch)
        && let Some(table) = arch_map
            .get(arch_key)
            .and_then(|arch_cfg| arch_cfg.libs.as_ref())
            .and_then(|libs| libs.get(library_name))
            .and_then(|inst| inst.vendor_struct_types.as_ref())
    {
        validate_vendor_struct_type_table_keys(api_name, table)?;
    }

    // Priority 2 — api + library
    if let Some(table) = api_cfg
        .libs
        .as_ref()
        .and_then(|libs| libs.get(library_name))
        .and_then(|inst| inst.vendor_struct_types.as_ref())
    {
        validate_vendor_struct_type_table_keys(api_name, table)?;
    }

    Ok(())
}

/// Shared grammar and value check for one `vendor_struct_types` table, used
/// by both priority levels in [`validate_vendor_struct_type_keys`]. Mirrors
/// `j2534-0404-service::config::parse_vendor_ioctl_entry`'s identical
/// `vendor_ioctls` key grammar check (ADR-219, as amended) -- the sibling bug
/// this function closes for `vendor_struct_types` (ADR-218). Also enforces
/// [`VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES`] on each entry's configured
/// size (Codex review, PR #133 twelfth round) -- see that constant's doc
/// comment for why this is a startup-rejection ceiling, not an allocation
/// cap.
fn validate_vendor_struct_type_table_keys(
    api_name: &str,
    table: &HashMap<String, u32>,
) -> Result<(), String> {
    for (key, &size) in table {
        let hex = key.strip_prefix("0x").filter(|hex| {
            hex.len() == 8
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        let Some(hex) = hex else {
            return Err(format!(
                "config.apis.{api_name}....vendor_struct_types key {key:?} must be formatted \
                 \"0x<8 lowercase hex digits>\" (the struct_type in hex, ADR-218)"
            ));
        };
        // ComParamStructType values 1-3 (PDU_CPST_SESSION_TIMING/
        // ACCESS_TIMING/TLS_VERSION_AND_CIPHER) are the three STANDARD
        // struct types -- their entry size is always known via `size_of`
        // (`iso22900-service::convert::standard_struct_entry_size`), and
        // `vendor_struct_from_proto` explicitly rejects them under the
        // vendor-specific write path. A `vendor_struct_types` entry for one
        // of these values is therefore never consultable by either read or
        // write -- reject it here too, matching the analogous
        // out-of-domain rejection `vendor_ioctls` already applies for a
        // cmd_id below its own valid (vendor) range.
        if let Ok(struct_type) = u32::from_str_radix(hex, 16)
            && (1..=3).contains(&struct_type)
        {
            return Err(format!(
                "config.apis.{api_name}....vendor_struct_types key {key:?} names a standard \
                 ComParamStructType (0x1-0x3, ADR-218) -- its entry size is always known and \
                 this table is never consulted for it; only vendor-specific struct types \
                 (any value outside 0x1-0x3) belong here"
            ));
        }
        // Reject an infeasibly large configured size before it can ever
        // reach `BorrowedVendorSpecificStructArray::bytes`'s unchecked
        // multiplication by the native entry count at request time -- see
        // `VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES`'s own doc comment for
        // why this is a rejection ceiling, not an allocation cap (Codex
        // review, PR #133 twelfth round). `0` (the "not configured at this
        // level" sentinel `find_vendor_struct_type_size` already treats
        // specially) is always within the ceiling and never rejected here.
        if size > VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES {
            return Err(format!(
                "config.apis.{api_name}....vendor_struct_types key {key:?} entry size \
                 ({size}) exceeds the {VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES}-byte sanity \
                 ceiling (ADR-218, as amended); raise \
                 VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES if a real vendor struct genuinely \
                 needs more"
            ));
        }
    }
    Ok(())
}

/// Return the entire configured `vendor_ioctls` table for the given service
/// instance, if one is present in the config file (ADR-219, as amended).
///
/// Applies the following priority hierarchy (highest first), the same
/// two-level shape [`find_library_path`]/[`find_vendor_struct_type_size`]
/// use and [`find_modules`]'s own whole-collection precedence (the winning
/// level's table is returned as a whole -- entries are never merged
/// key-by-key across levels):
///
/// 1. `config.apis.<api>.arch.<arch>.libs.<lib>.vendor_ioctls` (Windows only)
/// 2. `config.apis.<api>.libs.<lib>.vendor_ioctls`
///
/// Unlike [`find_vendor_struct_type_size`] (a per-key accessor, called once
/// per resolution at request time), this returns the whole table so
/// `j2534-0404-service` can load and fail-fast validate it once at startup
/// (mirroring the [`find_modules`]/`can_channel_mode` precedent) instead of
/// re-reading and re-parsing `config.toml` on every vendor IOCTL dispatch --
/// every vendor IOCTL's config lookup happens before any lock is taken
/// (ADR-219, as amended), so it must be cheap.
///
/// Each key is `cmd_id` formatted as `"0x<8 lowercase hex digits>"`, the
/// same grammar [`find_vendor_struct_type_size`] uses for `struct_type`.
/// Like [`find_library_path`], there is no api-level or root-level fallback:
/// a `vendor_ioctls` table only makes sense tied to a specific library.
///
/// Returns `Ok(None)` when the config file loads cleanly but no
/// `vendor_ioctls` table is configured for this library at all, in which
/// case the caller (`j2534-0404-service`) treats every `cmd_id` as
/// unconfigured -- any such request, buffer-carrying or not, is rejected
/// `FAILED_PRECONDITION` (ADR-219, as amended). This config table is the
/// sole source of a vendor IOCTL command's native contract.
///
/// Returns `Err` when the config file exists but cannot be read or parsed
/// -- like [`find_modules`] (and unlike every lenient per-key lookup
/// elsewhere in this module), this must NOT be silently treated as "no
/// table configured": `VendorIoctlConfigEntry`'s `#[serde(deny_unknown_fields)]`
/// means a misspelled `input_required`/`input_bytes`-class field makes the
/// WHOLE document fail to parse, and if that failure were swallowed into
/// "nothing configured" here, the typo would surface only as every vendor
/// `cmd_id` becoming unconfigured at runtime -- silent and confusing, not
/// the "fails at startup naming the typo" this table's every other grammar
/// check already achieves (Codex review, PR #133 eighth round).
pub fn find_vendor_ioctls(
    api_name: &str,
    arch: Option<&str>,
    library_name: &str,
) -> Result<Option<HashMap<String, VendorIoctlConfigEntry>>, ConfigFileError> {
    let toml = try_load_toml_config()?;
    let Some(mut api_cfg) = toml
        .config
        .and_then(|root| root.apis)
        .and_then(|mut apis| apis.remove(api_name))
    else {
        return Ok(None);
    };

    // Priority 1 — arch + library (Windows only)
    if let (Some(arch_key), Some(arch_map)) = (arch, api_cfg.arch.as_mut())
        && let Some(table) = arch_map
            .remove(arch_key)
            .and_then(|arch_cfg| arch_cfg.libs)
            .and_then(|mut libs| libs.remove(library_name))
            .and_then(|inst| inst.vendor_ioctls)
    {
        return Ok(Some(table));
    }

    // Priority 2 — api + library
    Ok(api_cfg
        .libs
        .and_then(|mut libs| libs.remove(library_name))
        .and_then(|inst| inst.vendor_ioctls))
}

/// Return the configured `can_channel_mode` string for the given service
/// instance, if one is present in the config file.
///
/// Applies the following priority hierarchy (highest first):
///
/// 1. `config.apis.<api>.arch.<arch>.libs.<lib>.can_channel_mode` (Windows only)
/// 2. `config.apis.<api>.libs.<lib>.can_channel_mode`
/// 3. `config.apis.<api>.can_channel_mode`
///
/// The value is returned as an opaque string; interpretation and validation
/// of the mode names is the consuming service's responsibility
/// (`j2534-0404-service` accepts `"dual-channel"`, `"single-channel"`, and
/// `"software-isotp"` — see ADR-046).
///
/// Returns `None` when no mode is configured, in which case the caller
/// should apply its own default.
pub fn find_can_channel_mode(
    api_name: &str,
    arch: Option<&str>,
    library_name: &str,
) -> Option<String> {
    let toml = load_toml_config();
    let mut api_cfg = toml.config?.apis?.remove(api_name)?;

    // Priority 1 — arch + library (Windows only)
    if let (Some(arch_key), Some(arch_map)) = (arch, api_cfg.arch.as_mut())
        && let Some(mode) = arch_map
            .remove(arch_key)
            .and_then(|arch_cfg| arch_cfg.libs)
            .and_then(|mut libs| libs.remove(library_name))
            .and_then(|inst| inst.can_channel_mode)
    {
        return Some(mode);
    }

    // Priority 2 — api + library
    if let Some(mode) = api_cfg
        .libs
        .as_mut()
        .and_then(|libs| libs.remove(library_name))
        .and_then(|inst| inst.can_channel_mode)
    {
        return Some(mode);
    }

    // Priority 3 — api-level default
    api_cfg.can_channel_mode
}

/// Return the configured `modules` array for the given service instance, if
/// one is present in the config file (ADR-106).
///
/// Applies the following priority hierarchy (highest first):
///
/// 1. `config.apis.<api>.arch.<arch>.libs.<lib>.modules` (Windows only)
/// 2. `config.apis.<api>.libs.<lib>.modules`
///
/// Unlike [`find_logging_config`], there is no api-level or root-level
/// fallback: a `modules` list only makes sense tied to a specific library
/// name, so entries without a `libs.<lib>` key are never consulted (same
/// contract as [`find_library_path`]).
///
/// Returns `Ok(None)` when no `modules` key is configured, in which case the
/// caller should fall back to today's single-synthetic-module behavior. A
/// present-but-empty array (`modules = []`) is returned as
/// `Ok(Some(vec![]))` — validating that case is the consuming service's
/// responsibility.
///
/// Returns `Err` when the config file exists but cannot be read or parsed.
/// Unlike every other lookup in this module, the caller must NOT treat that
/// as "modules absent": a malformed document makes the real modules answer
/// unknowable, and silently falling back to "not configured" here would
/// defeat module selection's entire purpose (see ADR-107 addendum (j)).
pub fn find_modules(
    api_name: &str,
    arch: Option<&str>,
    library_name: &str,
) -> Result<Option<Vec<ModuleConfigEntry>>, ConfigFileError> {
    let toml = try_load_toml_config()?;
    let Some(mut api_cfg) = toml
        .config
        .and_then(|root| root.apis)
        .and_then(|mut apis| apis.remove(api_name))
    else {
        return Ok(None);
    };

    // Priority 1 — arch + library (Windows only)
    if let (Some(arch_key), Some(arch_map)) = (arch, api_cfg.arch.as_mut())
        && let Some(modules) = arch_map
            .remove(arch_key)
            .and_then(|arch_cfg| arch_cfg.libs)
            .and_then(|mut libs| libs.remove(library_name))
            .and_then(|inst| inst.modules)
    {
        return Ok(Some(modules));
    }

    // Priority 2 — api + library
    Ok(api_cfg
        .libs
        .and_then(|mut libs| libs.remove(library_name))
        .and_then(|inst| inst.modules))
}

/// Returns the library names configured with a `library_path` under
/// `config.apis.<api>` in the config file, across both the api-level `libs`
/// table and every arch-level `libs` table (Windows only). Sorted and
/// deduplicated.
///
/// Entries without a `library_path` (e.g. logging-only entries) are not
/// included, since they carry nothing a caller could use to start or list
/// the library.
pub fn list_configured_libraries(api_name: &str) -> Vec<String> {
    list_configured_library_paths(api_name)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Returns every `(library_name, library_path)` pair configured under
/// `config.apis.<api>` in the config file, across both the api-level `libs`
/// table and every arch-level `libs` table (Windows only). Sorted by name
/// and deduplicated.
///
/// Entries without a `library_path` (e.g. logging-only entries) are not
/// included. Unlike [`find_library_path`], this does not apply the
/// arch-then-api priority for a specific runtime arch — it is meant for
/// discovery/display of everything the config file declares. If the same
/// name is configured with different paths at multiple levels (e.g. once
/// api-level, once under a specific `arch`), the lexicographically-first
/// path is kept.
pub fn list_configured_library_paths(api_name: &str) -> Vec<(String, PathBuf)> {
    let toml = load_toml_config();
    let Some(api_cfg) = toml
        .config
        .and_then(|root| root.apis)
        .and_then(|mut apis| apis.remove(api_name))
    else {
        return Vec::new();
    };

    let mut entries = Vec::new();
    if let Some(libs) = api_cfg.libs {
        entries.extend(
            libs.into_iter()
                .filter_map(|(name, inst)| inst.library_path.map(|p| (name, PathBuf::from(p)))),
        );
    }
    if let Some(arch_map) = api_cfg.arch {
        for arch_cfg in arch_map.into_values() {
            if let Some(libs) = arch_cfg.libs {
                entries.extend(libs.into_iter().filter_map(|(name, inst)| {
                    inst.library_path.map(|p| (name, PathBuf::from(p)))
                }));
            }
        }
    }

    entries.sort();
    entries.dedup_by(|a, b| a.0 == b.0);
    entries
}

/// Returns the `vci-service-manager` runtime settings from `config.manager`
/// in the config file (see ADR-073).
///
/// Unlike the api-scoped lookups above, there is no priority hierarchy to
/// resolve: `config.manager` is a single flat table. Missing fields, or a
/// missing `config.manager` table entirely, are returned as `None`; the
/// caller applies its own defaults.
pub fn manager_config() -> ManagerConfig {
    load_toml_config()
        .config
        .and_then(|root| root.manager)
        .unwrap_or_default()
}

/// Fixed, platform-specific system config directory for content that must
/// remain trust-root-verifiable (ADR-226 SS3) regardless of which
/// `config-root-*` Cargo feature this build happens to be compiled with.
///
/// This is deliberately NOT `config_root()`: two of `config_root()`'s four
/// Windows `config-root-win-*` options (`local-app-data`, `roaming-app-data`)
/// resolve to a PER-USER directory (`%LOCALAPPDATA%`/`%APPDATA%`) the
/// running process's own identity owns outright, which permanently fails
/// `vci-service-manager`'s own trust-root check (`AllowlistState::
/// verify_trust_root` requires the checked path to be owned by neither, and
/// writable by neither, the manager's own effective identity) -- an
/// admin-oriented Windows option isn't even any better: applying that same
/// check to an ordinary path under `config_root()`'s other two Windows
/// options (`FOLDERID_ProgramFiles`/`FOLDERID_ProgramFilesX86`) or its
/// non-Windows default root also failed when this function was written (it was the bare
/// filesystem root `/` before ADR-228 moved it to `/etc`), for a second, independent reason
/// unrelated to which folder was picked -- see `vci-service-manager`'s own Windows
/// `AccessCheck` mask fix (ADR-226 SS3 amendment) and its macOS residual (`/` is a sealed,
/// read-only system volume on Catalina+, and `/etc` is a symlink there). `system_config_dir()` sidesteps all of that by
/// always resolving to one FIXED, admin-writable-only location per
/// platform, never selected by a Cargo feature. Currently the sole caller is
/// `vci-service-manager`, to locate `vci-clients.toml` (ADR-226 SS3).
///
/// No Cargo feature gating (compiles identically regardless of which
/// `config-root-*` features are active) and no `#[cfg(test)]` override hook
/// (unlike `config_root()`, which has one for its own existing callers) --
/// this function's only caller always wants the real, unconditional value.
///
/// - Windows: `%ProgramData%\vci-service-launcher` (`FOLDERID_ProgramData`,
///   via [`known_folder`], always -- independent of whichever
///   `config-root-win-*` feature `config_root()` itself would pick).
/// - macOS: `/private/etc/vci-service-launcher`, spelled through `/private`
///   rather than `/etc` because `/etc` is itself a symlink to `/private/etc`
///   on macOS, and `vci-service-manager`'s own `reject_if_symlink` ancestor
///   check would otherwise reject the very first ancestor it walks.
/// - Every other Unix: `/etc/vci-service-launcher`.
pub fn system_config_dir() -> PathBuf {
    fixed_system_root().join("vci-service-launcher")
}

// ── Test-only config_root() override ───────────────────────────────────────

#[cfg(test)]
mod test_support {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    thread_local! {
        static OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    /// RAII guard that restores the previous override value on drop.
    ///
    /// Not `Send`/`Sync`; only ever used within the thread that created it.
    /// Each `#[test]` runs on its own OS thread under the default cargo test
    /// harness, so concurrent tests never see each other's override.
    pub(super) struct RootOverrideGuard {
        previous: Option<PathBuf>,
    }

    impl Drop for RootOverrideGuard {
        fn drop(&mut self) {
            OVERRIDE.with(|cell| *cell.borrow_mut() = self.previous.take());
        }
    }

    /// Scope `config_root()` to `root` for the current thread until the
    /// returned guard is dropped.
    pub(super) fn set(root: &Path) -> RootOverrideGuard {
        let previous = OVERRIDE.with(|cell| cell.replace(Some(root.to_path_buf())));
        RootOverrideGuard { previous }
    }

    pub(super) fn current_override() -> Option<PathBuf> {
        OVERRIDE.with(|cell| cell.borrow().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Scope `config_root()` to `dir` and write `contents` as the config
    /// file at the path `config_file_path()` resolves to within it.
    fn with_config(dir: &std::path::Path, contents: &str) -> test_support::RootOverrideGuard {
        let guard = test_support::set(dir);
        let full_path = config_file_path();
        fs::create_dir_all(full_path.parent().unwrap()).expect("create config dir");
        fs::write(&full_path, contents).expect("write test config.toml");
        guard
    }

    #[test]
    fn missing_config_file_returns_default() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = test_support::set(temp.path());
        let cfg = find_logging_config("iso22900", None, "somelib");
        assert!(cfg.level.is_none());
    }

    #[test]
    fn root_level_applies_when_nothing_more_specific_matches() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.logging]
            level = "warn"
        "#,
        );
        let cfg = find_logging_config("iso22900", None, "somelib");
        assert_eq!(cfg.level.as_deref(), Some("warn"));
    }

    #[test]
    fn api_plus_lib_outranks_api_level() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.logging]
            level = "warn"

            [config.apis.iso22900.logging]
            level = "info"

            [config.apis.iso22900.libs."mylib".logging]
            level = "trace"
        "#,
        );
        let cfg = find_logging_config("iso22900", None, "mylib");
        assert_eq!(cfg.level.as_deref(), Some("trace"));
        // A different library under the same api falls back to api-level.
        let other = find_logging_config("iso22900", None, "otherlib");
        assert_eq!(other.level.as_deref(), Some("info"));
    }

    #[test]
    fn arch_plus_lib_outranks_everything_else() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.logging]
            level = "warn"

            [config.apis.iso22900.logging]
            level = "info"

            [config.apis.iso22900.libs."mylib".logging]
            level = "debug"

            [config.apis.iso22900.arch.x86_64.logging]
            level = "error"

            [config.apis.iso22900.arch.x86_64.libs."mylib".logging]
            level = "trace"
        "#,
        );
        // Priority 1 wins over 2, 3, 4, 5.
        let cfg = find_logging_config("iso22900", Some("x86_64"), "mylib");
        assert_eq!(cfg.level.as_deref(), Some("trace"));
        // Without an arch match (arch=None), falls through to priority 3 (api+lib).
        let no_arch = find_logging_config("iso22900", None, "mylib");
        assert_eq!(no_arch.level.as_deref(), Some("debug"));
    }

    /// The release defaults (ADR-228): the configuration file under `nekoguruma` in the
    /// selected root, and the registration definitions under `nekoguruma` in the fixed system
    /// directory, whatever root a test or a `config-root-*` feature selects. A developer who
    /// exported either variable while building gets a different embedded value; the test then
    /// says so and checks nothing.
    #[test]
    fn default_locations_are_under_nekoguruma() {
        let defaults = env!("VCI_CONFIG_PATH") == "nekoguruma/config.toml"
            && env!("NGR_J2534_DEFINITION_DIR") == "nekoguruma/j2534";
        // Cargo puts `rustc-env` values into the test process's environment as well, so a
        // variable holding the embedded value is no override.
        let overridden = |name: &str, embedded: &str| {
            std::env::var_os(name).is_some_and(|value| !value.is_empty() && value != embedded)
        };
        let runtime_overrides = overridden("VCI_CONFIG_PATH", env!("VCI_CONFIG_PATH"))
            || overridden("NGR_J2534_DEFINITION_DIR", env!("NGR_J2534_DEFINITION_DIR"));
        if !defaults || runtime_overrides {
            eprintln!("skipped: VCI_CONFIG_PATH or NGR_J2534_DEFINITION_DIR is set");
            return;
        }
        let root = Path::new("/some/root");
        let _guard = test_support::set(root);
        assert_eq!(config_file_path(), root.join("nekoguruma/config.toml"));
        // The selected root does not move the definitions.
        assert_eq!(
            j2534_definition_dir(),
            fixed_system_root().join("nekoguruma/j2534")
        );
    }

    /// An unset or empty override leaves the build-time location in place, for both
    /// `VCI_CONFIG_PATH` and `NGR_J2534_DEFINITION_DIR`.
    #[cfg(debug_assertions)]
    #[test]
    fn an_empty_runtime_override_counts_as_unset() {
        assert_eq!(runtime_override(None), None);
        assert_eq!(runtime_override(Some("".into())), None);
        assert_eq!(
            runtime_override(Some("/tmp/dev.toml".into())),
            Some(PathBuf::from("/tmp/dev.toml"))
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_unix_roots_are_etc() {
        assert_eq!(fixed_system_root(), PathBuf::from("/etc"));
        #[cfg(not(feature = "config-root-exe-dir"))]
        assert_eq!(config_root_impl(), PathBuf::from("/etc"));
    }

    #[test]
    fn absolute_config_path_bypasses_root_resolution() {
        let temp = tempfile::tempdir().expect("create temp dir");
        // Point config_root() at a directory that is never read, proving the
        // absolute path below takes priority over it.
        let poison_root = temp.path().join("should-not-be-used");
        let _guard = test_support::set(&poison_root);

        let config_file = temp.path().join("absolute-config.toml");
        assert_eq!(resolve_config_path(&config_file), config_file);
    }

    #[test]
    fn unparseable_toml_falls_back_to_default() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(temp.path(), "this is not valid toml {{{");
        let cfg = find_logging_config("iso22900", None, "somelib");
        assert!(cfg.level.is_none());
    }

    #[test]
    fn missing_library_path_returns_none() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = test_support::set(temp.path());
        assert!(find_library_path("j2534-0404", None, "OpenPort2").is_none());
    }

    #[test]
    fn library_path_returns_none_when_lib_has_no_path() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.j2534-0404.libs."OpenPort2".logging]
            level = "debug"
        "#,
        );
        assert!(find_library_path("j2534-0404", None, "OpenPort2").is_none());
    }

    #[test]
    fn api_plus_lib_library_path_is_found() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.j2534-0404.libs."OpenPort2"]
            library_path = "/opt/vci/openport2.so"
        "#,
        );
        let path = find_library_path("j2534-0404", None, "OpenPort2");
        assert_eq!(path, Some(PathBuf::from("/opt/vci/openport2.so")));
        // A different library name under the same api is not affected.
        assert!(find_library_path("j2534-0404", None, "other-lib").is_none());
    }

    #[test]
    fn arch_plus_lib_library_path_outranks_api_plus_lib() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.j2534-0404.libs."OpenPort2"]
            library_path = "/opt/vci/openport2-generic.so"

            [config.apis.j2534-0404.arch.x86_64.libs."OpenPort2"]
            library_path = "/opt/vci/openport2-x86_64.so"
        "#,
        );
        let arch_specific = find_library_path("j2534-0404", Some("x86_64"), "OpenPort2");
        assert_eq!(
            arch_specific,
            Some(PathBuf::from("/opt/vci/openport2-x86_64.so"))
        );
        // Without an arch match, falls back to the api+lib entry.
        let no_arch = find_library_path("j2534-0404", None, "OpenPort2");
        assert_eq!(
            no_arch,
            Some(PathBuf::from("/opt/vci/openport2-generic.so"))
        );
    }

    #[test]
    fn missing_modules_returns_none() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = test_support::set(temp.path());
        assert!(
            find_modules("j2534-0404", None, "OpenPort2")
                .expect("config parses")
                .is_none()
        );
    }

    #[test]
    fn api_plus_lib_modules_is_found() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [[config.apis.j2534-0404.libs."OpenPort2".modules]]
            label = "Bench 1"
            pname = "USB:1"

            [[config.apis.j2534-0404.libs."OpenPort2".modules]]
            label = "Bench 2"
            pname = "USB:2"
        "#,
        );
        let modules = find_modules("j2534-0404", None, "OpenPort2")
            .expect("config parses")
            .expect("modules configured");
        assert_eq!(modules.len(), 2);
        assert_eq!(modules[0].label, "Bench 1");
        assert_eq!(modules[0].pname, "USB:1");
        assert_eq!(modules[1].label, "Bench 2");
        assert_eq!(modules[1].pname, "USB:2");
        // A different library name under the same api is not affected.
        assert!(
            find_modules("j2534-0404", None, "other-lib")
                .expect("config parses")
                .is_none()
        );
    }

    #[test]
    fn empty_modules_array_is_returned_as_some_empty() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.j2534-0404.libs."OpenPort2"]
            modules = []
        "#,
        );
        assert_eq!(
            find_modules("j2534-0404", None, "OpenPort2").expect("config parses"),
            Some(Vec::new())
        );
    }

    #[test]
    fn arch_plus_lib_modules_outranks_api_plus_lib() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [[config.apis.j2534-0404.libs."OpenPort2".modules]]
            label = "Generic"
            pname = "USB:0"

            [[config.apis.j2534-0404.arch.x86_64.libs."OpenPort2".modules]]
            label = "Arch-specific"
            pname = "USB:9"
        "#,
        );
        let arch_specific = find_modules("j2534-0404", Some("x86_64"), "OpenPort2")
            .expect("config parses")
            .expect("arch-specific modules configured");
        assert_eq!(arch_specific.len(), 1);
        assert_eq!(arch_specific[0].label, "Arch-specific");
        // Without an arch match, falls back to the api+lib entry.
        let no_arch = find_modules("j2534-0404", None, "OpenPort2")
            .expect("config parses")
            .expect("api+lib modules configured");
        assert_eq!(no_arch.len(), 1);
        assert_eq!(no_arch[0].label, "Generic");
    }

    #[test]
    fn malformed_modules_entry_is_an_error_not_none() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [[config.apis.j2534-0404.libs."OpenPort2".modules]]
            label = "Missing pname"
        "#,
        );
        let err = find_modules("j2534-0404", None, "OpenPort2")
            .expect_err("malformed modules entry must be a parse error, not None");
        assert!(matches!(err, ConfigFileError::Parse { .. }));
    }

    #[test]
    fn unreadable_or_unparseable_document_is_an_error_for_find_modules() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(temp.path(), "this is not valid toml {{{");
        let err = find_modules("j2534-0404", None, "OpenPort2")
            .expect_err("unparseable document must be a parse error, not None");
        assert!(matches!(err, ConfigFileError::Parse { .. }));
    }

    #[test]
    fn missing_can_channel_mode_returns_none() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = test_support::set(temp.path());
        assert!(find_can_channel_mode("j2534-0404", None, "OpenPort2").is_none());
    }

    #[test]
    fn api_level_can_channel_mode_applies_to_all_libs() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.j2534-0404]
            can_channel_mode = "software-isotp"
        "#,
        );
        assert_eq!(
            find_can_channel_mode("j2534-0404", None, "any-lib").as_deref(),
            Some("software-isotp")
        );
    }

    #[test]
    fn lib_level_can_channel_mode_outranks_api_level() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.j2534-0404]
            can_channel_mode = "single-channel"

            [config.apis.j2534-0404.libs."OpenPort2"]
            can_channel_mode = "dual-channel"
        "#,
        );
        assert_eq!(
            find_can_channel_mode("j2534-0404", None, "OpenPort2").as_deref(),
            Some("dual-channel")
        );
        // A different library falls back to the api-level default.
        assert_eq!(
            find_can_channel_mode("j2534-0404", None, "other-lib").as_deref(),
            Some("single-channel")
        );
    }

    #[test]
    fn arch_plus_lib_can_channel_mode_outranks_api_plus_lib() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.j2534-0404.libs."OpenPort2"]
            can_channel_mode = "single-channel"

            [config.apis.j2534-0404.arch.x86_64.libs."OpenPort2"]
            can_channel_mode = "dual-channel"
        "#,
        );
        assert_eq!(
            find_can_channel_mode("j2534-0404", Some("x86_64"), "OpenPort2").as_deref(),
            Some("dual-channel")
        );
        assert_eq!(
            find_can_channel_mode("j2534-0404", None, "OpenPort2").as_deref(),
            Some("single-channel")
        );
    }

    #[test]
    fn list_configured_libraries_returns_empty_for_missing_config() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = test_support::set(temp.path());
        assert!(list_configured_libraries("iso22900").is_empty());
    }

    #[test]
    fn list_configured_libraries_excludes_entries_without_a_path() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.libs."HasPath"]
            library_path = "/opt/vci/haspath.so"

            [config.apis.iso22900.libs."LoggingOnly".logging]
            level = "debug"
        "#,
        );
        assert_eq!(
            list_configured_libraries("iso22900"),
            vec!["HasPath".to_string()]
        );
    }

    #[test]
    fn list_configured_libraries_combines_api_and_arch_entries() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.libs."ApiLevelLib"]
            library_path = "/opt/vci/api-level.so"

            [config.apis.iso22900.arch.x86_64.libs."ArchOnlyLib"]
            library_path = "/opt/vci/arch-only.so"
        "#,
        );
        assert_eq!(
            list_configured_libraries("iso22900"),
            vec!["ApiLevelLib".to_string(), "ArchOnlyLib".to_string()]
        );
    }

    #[test]
    fn list_configured_library_paths_returns_empty_for_missing_config() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = test_support::set(temp.path());
        assert!(list_configured_library_paths("iso22900").is_empty());
    }

    #[test]
    fn list_configured_library_paths_includes_the_path() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.libs."HasPath"]
            library_path = "/opt/vci/haspath.so"

            [config.apis.iso22900.libs."LoggingOnly".logging]
            level = "debug"

            [config.apis.iso22900.arch.x86_64.libs."ArchOnlyLib"]
            library_path = "/opt/vci/arch-only.so"
        "#,
        );
        assert_eq!(
            list_configured_library_paths("iso22900"),
            vec![
                (
                    "ArchOnlyLib".to_string(),
                    PathBuf::from("/opt/vci/arch-only.so")
                ),
                ("HasPath".to_string(), PathBuf::from("/opt/vci/haspath.so")),
            ]
        );
    }

    #[test]
    fn manager_config_missing_config_file_returns_none_fields() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = test_support::set(temp.path());
        let cfg = manager_config();
        assert!(cfg.bind.is_none());
        assert!(cfg.root_path.is_none());
        assert!(cfg.ipc.is_none());
    }

    #[test]
    fn manager_config_reads_configured_values() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.manager]
            bind = "0.0.0.0:9090"
            root_path = "/vci"
            ipc = "/run/vci-service-manager.sock"
        "#,
        );
        let cfg = manager_config();
        assert_eq!(cfg.bind.as_deref(), Some("0.0.0.0:9090"));
        assert_eq!(cfg.root_path.as_deref(), Some("/vci"));
        assert_eq!(cfg.ipc.as_deref(), Some("/run/vci-service-manager.sock"));
    }

    #[test]
    fn manager_config_absent_table_returns_none_fields() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.logging]
            level = "warn"
        "#,
        );
        let cfg = manager_config();
        assert!(cfg.bind.is_none());
        assert!(cfg.root_path.is_none());
        assert!(cfg.ipc.is_none());
    }

    #[test]
    fn missing_vendor_struct_type_size_returns_none() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = test_support::set(temp.path());
        assert!(find_vendor_struct_type_size("iso22900", None, "TestLib", 0x8000_0001).is_none());
    }

    #[test]
    fn api_plus_lib_vendor_struct_type_size_is_found() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x80000001" = 12
        "#,
        );
        assert_eq!(
            find_vendor_struct_type_size("iso22900", None, "TestLib", 0x8000_0001),
            Some(12)
        );
        // A different struct type under the same library is not affected.
        assert!(find_vendor_struct_type_size("iso22900", None, "TestLib", 0x8000_0002).is_none());
        // A different library is not affected.
        assert!(find_vendor_struct_type_size("iso22900", None, "OtherLib", 0x8000_0001).is_none());
    }

    #[test]
    fn arch_plus_lib_vendor_struct_type_size_outranks_api_plus_lib() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x80000001" = 12

            [config.apis.iso22900.arch.x86_64.libs."TestLib".vendor_struct_types]
            "0x80000001" = 16
        "#,
        );
        assert_eq!(
            find_vendor_struct_type_size("iso22900", Some("x86_64"), "TestLib", 0x8000_0001),
            Some(16)
        );
        // Without an arch match, falls back to the api+lib entry.
        assert_eq!(
            find_vendor_struct_type_size("iso22900", None, "TestLib", 0x8000_0001),
            Some(12)
        );
    }

    #[test]
    fn a_zero_arch_level_vendor_struct_type_size_falls_back_to_api_plus_lib() {
        // `resolve_vendor_struct_entry_size` treats a configured `0` as
        // "not configured" (`size != 0` filter); this level's resolution
        // must not short-circuit on `0` before that filter ever runs, or a
        // zero placeholder at the arch level permanently shadows a real
        // nonzero value configured at the api+library level (Codex review,
        // PR #133 sixth round).
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x80000001" = 12

            [config.apis.iso22900.arch.x86_64.libs."TestLib".vendor_struct_types]
            "0x80000001" = 0
        "#,
        );
        assert_eq!(
            find_vendor_struct_type_size("iso22900", Some("x86_64"), "TestLib", 0x8000_0001),
            Some(12)
        );
    }

    #[test]
    fn an_oversized_arch_level_vendor_struct_type_size_falls_back_to_api_plus_lib() {
        // Mirrors the zero-arch-level test above, but for the ceiling
        // instead of the zero sentinel: find_vendor_struct_type_size must
        // filter out an oversized value at lookup time too, not only rely
        // on validate_vendor_struct_type_keys's startup check, since a
        // value edited into config.toml after startup would otherwise
        // bypass that check entirely (Codex review, PR #133 thirteenth
        // round).
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            &format!(
                r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x80000001" = 12

            [config.apis.iso22900.arch.x86_64.libs."TestLib".vendor_struct_types]
            "0x80000001" = {}
        "#,
                VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES + 1
            ),
        );
        assert_eq!(
            find_vendor_struct_type_size("iso22900", Some("x86_64"), "TestLib", 0x8000_0001),
            Some(12),
            "an oversized arch-level size must fall through to the api+lib value, not be returned"
        );
    }

    #[test]
    fn an_arch_level_vendor_struct_type_size_at_the_sanity_ceiling_is_returned() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            &format!(
                r#"
            [config.apis.iso22900.arch.x86_64.libs."TestLib".vendor_struct_types]
            "0x80000001" = {VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES}
        "#
            ),
        );
        assert_eq!(
            find_vendor_struct_type_size("iso22900", Some("x86_64"), "TestLib", 0x8000_0001),
            Some(VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES),
            "an arch-level size exactly at the ceiling must still be returned"
        );
    }

    #[test]
    fn an_oversized_api_level_vendor_struct_type_size_resolves_to_none() {
        // Unlike the arch level, there is no further level to fall through
        // to -- an oversized api+lib value resolves to None, the same
        // "unconfigured" outcome an absent entry already produces.
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            &format!(
                r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x80000001" = {}
        "#,
                VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES + 1
            ),
        );
        assert_eq!(
            find_vendor_struct_type_size("iso22900", None, "TestLib", 0x8000_0001),
            None
        );
    }

    #[test]
    fn validate_vendor_struct_type_keys_passes_when_table_absent() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = test_support::set(temp.path());
        assert!(validate_vendor_struct_type_keys("iso22900", None, "TestLib").is_ok());
    }

    #[test]
    fn validate_vendor_struct_type_keys_is_an_error_not_ok_for_a_malformed_config_file() {
        // Unlike every other lookup in this module, a broken config file
        // must NOT be silently treated as "nothing configured" here: doing
        // so would let a malformed `vendor_struct_types` table pass
        // validation while `Iso22900Service::new` proceeds to start up
        // (Codex review, PR #133 sixth round).
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(temp.path(), "not valid toml [[[");
        let err = validate_vendor_struct_type_keys("iso22900", None, "TestLib")
            .expect_err("a malformed config file must fail validation, not pass it silently");
        assert!(err.contains("failed to parse config file"));
    }

    #[test]
    fn validate_vendor_struct_type_keys_passes_for_canonical_keys() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x80000001" = 12

            [config.apis.iso22900.arch.x86_64.libs."TestLib".vendor_struct_types]
            "0x80000002" = 16
        "#,
        );
        assert!(validate_vendor_struct_type_keys("iso22900", Some("x86_64"), "TestLib").is_ok());
        assert!(validate_vendor_struct_type_keys("iso22900", None, "TestLib").is_ok());
    }

    #[test]
    fn validate_vendor_struct_type_keys_rejects_uppercase_key() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x8000000A" = 12
        "#,
        );
        let err = validate_vendor_struct_type_keys("iso22900", None, "TestLib")
            .expect_err("an uppercase hex key must be rejected");
        assert!(
            err.contains("0x8000000A"),
            "error must name the offending key: {err}"
        );
    }

    #[test]
    fn validate_vendor_struct_type_keys_rejects_wrong_width_key() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x800000a" = 12
        "#,
        );
        let err = validate_vendor_struct_type_keys("iso22900", None, "TestLib")
            .expect_err("a wrong-width hex key must be rejected");
        assert!(
            err.contains("0x800000a"),
            "error must name the offending key: {err}"
        );
    }

    #[test]
    fn validate_vendor_struct_type_keys_rejects_bad_arch_level_key_even_when_api_level_is_clean() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x80000001" = 12

            [config.apis.iso22900.arch.x86_64.libs."TestLib".vendor_struct_types]
            "0x8000000A" = 16
        "#,
        );
        let err = validate_vendor_struct_type_keys("iso22900", Some("x86_64"), "TestLib")
            .expect_err(
                "a bad arch-level key must be rejected even if the api-level table is clean",
            );
        assert!(
            err.contains("0x8000000A"),
            "error must name the offending key: {err}"
        );
    }

    #[test]
    fn validate_vendor_struct_type_keys_rejects_a_standard_struct_type_id() {
        // 0x1-0x3 (PDU_CPST_SESSION_TIMING/ACCESS_TIMING/TLS_VERSION_AND_CIPHER)
        // are the standard struct types: their entry size is always known
        // via size_of, and the write path explicitly rejects them under the
        // vendor-specific path -- an entry here for one of these values
        // would parse but never actually be consulted by either read or
        // write.
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x00000001" = 12
        "#,
        );
        let err = validate_vendor_struct_type_keys("iso22900", None, "TestLib")
            .expect_err("a standard ComParamStructType id (0x1-0x3) should be rejected");
        assert!(
            err.contains("0x00000001") && err.contains("standard"),
            "error must name the offending key and explain why: {err}"
        );
    }

    #[test]
    fn validate_vendor_struct_type_keys_accepts_ids_just_outside_the_standard_range() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x00000000" = 12
            "0x00000004" = 16
        "#,
        );
        assert!(validate_vendor_struct_type_keys("iso22900", None, "TestLib").is_ok());
    }

    #[test]
    fn validate_vendor_struct_type_keys_accepts_a_size_at_the_sanity_ceiling() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            &format!(
                r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x80000001" = {VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES}
        "#
            ),
        );
        assert!(
            validate_vendor_struct_type_keys("iso22900", None, "TestLib").is_ok(),
            "an entry size exactly at the ceiling must still be accepted"
        );
    }

    #[test]
    fn validate_vendor_struct_type_keys_rejects_a_size_above_the_sanity_ceiling() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            &format!(
                r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x80000001" = {}
        "#,
                VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES + 1
            ),
        );
        let err = validate_vendor_struct_type_keys("iso22900", None, "TestLib")
            .expect_err("an entry size one above the ceiling should be rejected at startup");
        assert!(
            err.contains("0x80000001") && err.contains("sanity ceiling"),
            "error must name the offending key and explain why: {err}"
        );
    }

    #[test]
    fn validate_vendor_struct_type_keys_rejects_a_size_of_u32_max() {
        // The exact scenario Codex's finding described: an absurd
        // syntactically-valid value that would otherwise reach an unbounded
        // allocation at request time.
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            &format!(
                r#"
            [config.apis.iso22900.libs."TestLib".vendor_struct_types]
            "0x80000001" = {}
        "#,
                u32::MAX
            ),
        );
        let err = validate_vendor_struct_type_keys("iso22900", None, "TestLib")
            .expect_err("u32::MAX should be rejected at startup");
        assert!(
            err.contains("sanity ceiling"),
            "error must explain why: {err}"
        );
    }

    #[test]
    fn validate_vendor_struct_type_keys_accepts_a_zero_size() {
        // 0 is the "not configured at this level" sentinel
        // find_vendor_struct_type_size already treats specially -- it must
        // never be rejected by the ceiling check.
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.iso22900.arch.x86_64.libs."TestLib".vendor_struct_types]
            "0x80000001" = 0
        "#,
        );
        assert!(validate_vendor_struct_type_keys("iso22900", Some("x86_64"), "TestLib").is_ok());
    }

    #[test]
    fn validate_vendor_struct_type_keys_rejects_an_oversized_arch_level_entry() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            &format!(
                r#"
            [config.apis.iso22900.arch.x86_64.libs."TestLib".vendor_struct_types]
            "0x80000001" = {}
        "#,
                VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES + 1
            ),
        );
        let err = validate_vendor_struct_type_keys("iso22900", Some("x86_64"), "TestLib")
            .expect_err("an oversized arch-level entry must be rejected too, not just api-level");
        assert!(
            err.contains("sanity ceiling"),
            "error must explain why: {err}"
        );
    }

    #[test]
    fn missing_vendor_ioctls_table_returns_none() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = test_support::set(temp.path());
        assert!(
            find_vendor_ioctls("j2534-0404", None, "TestLib")
                .expect("config parses")
                .is_none()
        );
    }

    #[test]
    fn vendor_ioctl_entry_rejects_an_unknown_field() {
        // #[serde(deny_unknown_fields)]: a misspelled safety-critical field
        // (here "input_requred" for "input_required") must fail to parse
        // rather than silently deserializing with the misspelled field's
        // #[serde(default)] value in effect (Codex review, PR #133 eighth
        // round).
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.j2534-0404.libs."TestLib".vendor_ioctls."0x00010002"]
            shape = "sbyte_array"
            input_requred = true
        "#,
        );
        let err = find_vendor_ioctls("j2534-0404", None, "TestLib")
            .expect_err("an unknown vendor_ioctls field must fail to parse, not deserialize silently with defaults");
        assert!(err.to_string().contains("failed to parse config file"));
    }

    #[test]
    fn find_vendor_ioctls_is_an_error_not_ok_for_a_malformed_config_file() {
        // Unlike the lenient per-key lookups elsewhere in this module, a
        // broken config file must propagate as Err here -- swallowing it
        // into "no table configured" would make a malformed vendor_ioctls
        // table look identical to an absent one, which is exactly the
        // silent-failure mode deny_unknown_fields above exists to avoid.
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(temp.path(), "not valid toml [[[");
        let err = find_vendor_ioctls("j2534-0404", None, "TestLib")
            .expect_err("a malformed config file must fail, not resolve to an absent table");
        assert!(err.to_string().contains("failed to parse config file"));
    }

    #[test]
    fn api_plus_lib_vendor_ioctls_table_is_found() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.j2534-0404.libs."TestLib".vendor_ioctls."0x00010001"]
            shape = "raw"
            input_bytes = 4
            output_bytes = 4

            [config.apis.j2534-0404.libs."TestLib".vendor_ioctls."0x00010002"]
            shape = "sbyte_array"
        "#,
        );
        let table = find_vendor_ioctls("j2534-0404", None, "TestLib")
            .expect("config parses")
            .expect("table should be present");
        assert_eq!(
            table.get("0x00010001"),
            Some(&VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 4,
                output_bytes: 4,
                input_required: false,
                output_required: false,
            })
        );
        assert_eq!(
            table.get("0x00010002"),
            Some(&VendorIoctlConfigEntry {
                shape: "sbyte_array".to_string(),
                input_bytes: 0,
                output_bytes: 0,
                input_required: false,
                output_required: false,
            })
        );
        // A different library is not affected.
        assert!(
            find_vendor_ioctls("j2534-0404", None, "OtherLib")
                .expect("config parses")
                .is_none()
        );
    }

    #[test]
    fn vendor_ioctl_entry_input_output_bytes_default_to_zero() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.j2534-0404.libs."TestLib".vendor_ioctls."0x00010002"]
            shape = "sbyte_array"
        "#,
        );
        let table = find_vendor_ioctls("j2534-0404", None, "TestLib")
            .expect("config parses")
            .expect("table should be present");
        assert_eq!(
            table.get("0x00010002"),
            Some(&VendorIoctlConfigEntry {
                shape: "sbyte_array".to_string(),
                input_bytes: 0,
                output_bytes: 0,
                input_required: false,
                output_required: false,
            })
        );
    }

    #[test]
    fn vendor_ioctl_entry_input_output_required_parse_and_default_to_false() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.j2534-0404.libs."TestLib".vendor_ioctls."0x00010002"]
            shape = "sbyte_array"
            input_required = true
            output_required = true

            [config.apis.j2534-0404.libs."TestLib".vendor_ioctls."0x00010003"]
            shape = "sbyte_array"
        "#,
        );
        let table = find_vendor_ioctls("j2534-0404", None, "TestLib")
            .expect("config parses")
            .expect("table should be present");
        assert_eq!(
            table.get("0x00010002"),
            Some(&VendorIoctlConfigEntry {
                shape: "sbyte_array".to_string(),
                input_bytes: 0,
                output_bytes: 0,
                input_required: true,
                output_required: true,
            })
        );
        assert_eq!(
            table.get("0x00010003"),
            Some(&VendorIoctlConfigEntry {
                shape: "sbyte_array".to_string(),
                input_bytes: 0,
                output_bytes: 0,
                input_required: false,
                output_required: false,
            })
        );
    }

    #[test]
    fn arch_plus_lib_vendor_ioctls_table_outranks_api_plus_lib_as_a_whole() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let _guard = with_config(
            temp.path(),
            r#"
            [config.apis.j2534-0404.libs."TestLib".vendor_ioctls."0x00010001"]
            shape = "raw"
            input_bytes = 4
            output_bytes = 4

            [config.apis.j2534-0404.arch.x86_64.libs."TestLib".vendor_ioctls."0x00010002"]
            shape = "sbyte_array"
        "#,
        );
        // The arch+lib table wins as a whole -- it does not also see the
        // api+lib table's "0x00010001" entry, mirroring find_modules's
        // whole-collection (never merged key-by-key) precedence.
        let arch_table = find_vendor_ioctls("j2534-0404", Some("x86_64"), "TestLib")
            .expect("config parses")
            .expect("arch table should be present");
        assert_eq!(
            arch_table.get("0x00010002"),
            Some(&VendorIoctlConfigEntry {
                shape: "sbyte_array".to_string(),
                input_bytes: 0,
                output_bytes: 0,
                input_required: false,
                output_required: false,
            })
        );
        assert!(!arch_table.contains_key("0x00010001"));
        // Without an arch match, falls back to the api+lib table.
        let api_table = find_vendor_ioctls("j2534-0404", None, "TestLib")
            .expect("config parses")
            .expect("api+lib table should be present");
        assert_eq!(
            api_table.get("0x00010001"),
            Some(&VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 4,
                output_bytes: 4,
                input_required: false,
                output_required: false,
            })
        );
    }

    /// `system_config_dir()` (ADR-226 SS3 amendment) resolves to a fixed
    /// path shape on the current test-host platform, unlike `config_root()`
    /// -- it has no `#[cfg(test)]` override hook and is never scoped by
    /// `test_support::set`, so this test asserts against the function's real
    /// return value directly. On Linux (this repo's CI/sandbox host) that's
    /// `/etc/vci-service-launcher`; see the function's own doc comment for
    /// the macOS (`/private/etc/vci-service-launcher`) and Windows
    /// (`%ProgramData%\vci-service-launcher`) cases, which cannot be
    /// exercised without a real host of that platform.
    ///
    /// This function is also deliberately NOT gated by any `config-root-*`
    /// Cargo feature (unlike `config_root()`, whose Windows resolution
    /// varies with which `config-root-win-*` feature is active) -- but that
    /// specific claim isn't practically assertable in one test binary, since
    /// cargo cannot rebuild under a different feature set mid-run; this test
    /// covers only the fixed-path-shape claim on whichever platform it
    /// actually runs on.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn system_config_dir_resolves_to_the_fixed_etc_path_on_linux() {
        assert_eq!(
            system_config_dir(),
            PathBuf::from("/etc/vci-service-launcher")
        );
    }
}

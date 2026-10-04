use std::{
    fmt, fs,
    io::{self, ErrorKind},
    path::PathBuf,
};

use tracing::{debug, warn};
use url::Url;

#[derive(Debug, Clone)]
pub struct PduLibraryInfo {
    pub short_name: String,
    pub description: Option<String>,
    pub supplier_name: Option<String>,
    pub library_path: PathBuf,
    pub module_description_path: Option<PathBuf>,
    pub cable_description_path: Option<PathBuf>,
    pub arch: LibraryArch,
    pub source: LibrarySource,
}

/// Where a [`PduLibraryInfo`] returned by [`enumerate_libraries`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibrarySource {
    /// Found via RDF-based auto-discovery only.
    Registry,
    /// Present only as a `library_path` entry in the config file (see
    /// [`enumerate_libraries`]); no matching RDF entry was found.
    Config,
    /// Found via RDF-based auto-discovery, with the config file additionally
    /// providing a `library_path` override for this library name — the
    /// `library_path` field reflects the configured override.
    Both,
}

#[derive(Debug)]
pub enum RegistryError {
    Io(io::Error),
    Xml(roxmltree::Error),
    Url(url::ParseError),
    InvalidUri(String),
    NotFound(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Io(e) => write!(f, "IO error: {e}"),
            RegistryError::Xml(e) => write!(f, "XML parse error: {e}"),
            RegistryError::Url(e) => write!(f, "URL parse error: {e}"),
            RegistryError::InvalidUri(u) => write!(f, "Invalid URI: {u}"),
            RegistryError::NotFound(msg) => write!(f, "Not found: {msg}"),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<io::Error> for RegistryError {
    fn from(e: io::Error) -> Self {
        if e.kind() == ErrorKind::NotFound {
            debug!(error = %e, "D-PDU API root description file not found");
            RegistryError::NotFound("D-PDU API root description file not found".to_string())
        } else {
            RegistryError::Io(e)
        }
    }
}
impl From<roxmltree::Error> for RegistryError {
    fn from(e: roxmltree::Error) -> Self {
        RegistryError::Xml(e)
    }
}
impl From<url::ParseError> for RegistryError {
    fn from(e: url::ParseError) -> Self {
        RegistryError::Url(e)
    }
}

macro_rules! define_arch {
    ($($(#[$attr:meta])* $v:ident$(($s:expr, $f:expr $(,)?))?),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum LibraryArch {
            Native,
            $($(#[$attr])* $v,)*
        }
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum RegistryViewMode {
            All,
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
        impl From<LibraryArch> for Option<&str> {
            fn from(mode: LibraryArch) -> Self {
                match mode {
                    $($(#[$attr])* LibraryArch::$v => [$($s,)? stringify!($v)].get(0).map(|s| *s),)*
                    LibraryArch::Native => None,
                }
            }
        }
        #[cfg(windows)]
        fn get_registry_flags(mode: RegistryViewMode) -> Option<u32> {
            match mode {
                $($(#[$attr])* RegistryViewMode::$v => [$($f)?].get(0).cloned(),)*
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
    #[cfg(windows)] #[cfg(target_arch = "x86_64")] W32("X86", winreg::enums::KEY_WOW64_32KEY),
    #[cfg(windows)] #[cfg(target_arch = "x86_64")] W64("X64", winreg::enums::KEY_WOW64_64KEY),
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

impl LibraryArch {
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

#[cfg(not(windows))]
fn root_description_file_path(_mode: RegistryViewMode) -> Result<PathBuf, RegistryError> {
    Ok("/etc/pdu_api_root.xml".into())
}

#[cfg(windows)]
fn root_description_file_path(mode: RegistryViewMode) -> Result<PathBuf, RegistryError> {
    use winreg::{
        RegKey,
        enums::{HKEY_LOCAL_MACHINE, KEY_READ},
    };
    Ok(PathBuf::from(
        RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags(
                "SOFTWARE\\D-PDU API",
                KEY_READ | get_registry_flags(mode).unwrap_or(0),
            )?
            .get_value::<String, &str>("Root File")?,
    ))
}

fn uri_to_path(uri: &str) -> Result<PathBuf, RegistryError> {
    let parsed = Url::parse(uri)?;
    if parsed.scheme() != "file" {
        return Err(RegistryError::InvalidUri(format!(
            "unsupported URI scheme '{}'; expected 'file'",
            parsed.scheme()
        )));
    }
    parsed.to_file_path().map_err(|_| {
        RegistryError::InvalidUri(format!(
            "failed to convert file URI '{uri}' to a filesystem path"
        ))
    })
}

/// Enumerate all available D-PDU API libraries from the RDF(s).
///
/// Returns a vector of PduLibraryInfo, sorted by short_name.
///
/// `RegistryViewMode::All` queries every architecture-specific view this
/// build can enumerate and combines the results. On a build that
/// enumerates no architecture-specific views at all (32-bit Windows, ARM64
/// Windows, or any non-Windows target), it queries the process's default
/// (`Native`) view instead — see ADR-223. A build that CAN see multiple
/// views (x86_64 Windows) still only sees those views' devices, not a
/// WOW64-detected sibling view; that gap is a documented residual, not
/// covered by `All` here.
pub fn enumerate_pdu_libraries(
    mode: RegistryViewMode,
) -> Result<Vec<PduLibraryInfo>, RegistryError> {
    if mode == RegistryViewMode::All {
        views_for_all(get_view_mode_list())
            .iter()
            .try_fold(Vec::new(), |mut acc, &view_mode| {
                match enumerate_pdu_libraries(view_mode) {
                    Ok(mut libs) => {
                        acc.append(&mut libs);
                    }
                    Err(RegistryError::NotFound(_)) => {
                        // If a specific view's RDF is not found, we can ignore it in "All" mode
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
                Ok(acc)
            })
    } else {
        let mode = if mode == RegistryViewMode::Native {
            get_native_view_mode()
        } else {
            mode
        };
        let rdf_path = root_description_file_path(mode)?;
        enumerate_pdu_libraries_from_rdf(&rdf_path, mode)
    }
}
fn enumerate_pdu_libraries_from_rdf(
    rdf_path: &PathBuf,
    mode: RegistryViewMode,
) -> Result<Vec<PduLibraryInfo>, RegistryError> {
    debug!(rdf_path = %rdf_path.display(), ?mode, "enumerating D-PDU libraries from RDF");
    let mut libraries = Vec::new();
    let xml = fs::read_to_string(rdf_path)?;
    let document = roxmltree::Document::parse(&xml)?;
    for pdu_api in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "MVCI_PDU_API")
    {
        let short_name = pdu_api
            .children()
            .find(|node| node.is_element() && node.tag_name().name() == "SHORT_NAME")
            .and_then(|node| node.text())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let description = pdu_api
            .children()
            .find(|node| node.is_element() && node.tag_name().name() == "DESCRIPTION")
            .and_then(|node| node.text())
            .map(|s| s.trim().to_owned());
        let supplier_name = pdu_api
            .children()
            .find(|node| node.is_element() && node.tag_name().name() == "SUPPLIER_NAME")
            .and_then(|node| node.text())
            .map(|s| s.trim().to_owned());
        let library_uri = pdu_api
            .children()
            .find(|node| node.is_element() && node.tag_name().name() == "LIBRARY_FILE")
            .and_then(|node| node.attribute("URI"));
        let module_description_uri = pdu_api
            .children()
            .find(|node| node.is_element() && node.tag_name().name() == "MODULE_DESCRIPTION_FILE")
            .and_then(|node| node.attribute("URI"));
        let cable_description_uri = pdu_api
            .children()
            .find(|node| node.is_element() && node.tag_name().name() == "CABLE_DESCRIPTION_FILE")
            .and_then(|node| node.attribute("URI"));

        if let (Some(short_name), Some(library_uri)) = (short_name, library_uri) {
            let library_path = uri_to_path(library_uri)?;
            let module_description_path = module_description_uri.map(uri_to_path).transpose()?;
            let cable_description_path = cable_description_uri.map(uri_to_path).transpose()?;
            debug!(
                short_name,
                library_path = %library_path.display(),
                "found D-PDU library"
            );
            libraries.push(PduLibraryInfo {
                short_name,
                description,
                supplier_name,
                library_path,
                module_description_path,
                cable_description_path,
                arch: mode.into(),
                source: LibrarySource::Registry,
            });
        }
    }
    debug!(
        count = libraries.len(),
        ?mode,
        "D-PDU library enumeration complete"
    );
    Ok(libraries)
}

pub fn find_pdu_libraries<Name: AsRef<str>>(
    short_name: &Name,
) -> Result<PduLibraryInfo, RegistryError> {
    debug!(short_name = short_name.as_ref(), "looking up D-PDU library");
    let result = enumerate_pdu_libraries(RegistryViewMode::Native)?
        .into_iter()
        .find(|lib| lib.short_name == short_name.as_ref())
        .ok_or_else(|| {
            RegistryError::NotFound(format!("PDU library '{}' not found", short_name.as_ref()))
        });
    if let Err(RegistryError::NotFound(ref msg)) = result {
        warn!(msg, "D-PDU library not found");
    }
    result
}
pub fn find_pdu_libraries_on_registry<Name: AsRef<str>>(
    short_name: &Name,
    mode: RegistryViewMode,
) -> Result<PduLibraryInfo, RegistryError> {
    debug!(
        short_name = short_name.as_ref(),
        ?mode,
        "looking up D-PDU library"
    );
    let result = enumerate_pdu_libraries(mode)?
        .into_iter()
        .find(|lib| lib.short_name == short_name.as_ref())
        .ok_or_else(|| {
            RegistryError::NotFound(format!("PDU library '{}' not found", short_name.as_ref()))
        });
    if let Err(RegistryError::NotFound(ref msg)) = result {
        warn!(msg, "D-PDU library not found");
    }
    result
}

/// Returns a configured `library_path` override for `library_name` from the
/// shared `config.toml` (`config.apis.iso22900...`), if present.
///
/// Applies a 2-level priority (highest first):
/// 1. `config.apis.iso22900.arch.<arch>.libs.<lib>.library_path` (Windows only)
/// 2. `config.apis.iso22900.libs.<lib>.library_path`
///
/// Returns `None` when no override is configured, in which case the caller
/// should fall back to RDF-based auto-discovery (`find_pdu_libraries`).
pub fn find_library_path(arch: Option<&str>, library_name: &str) -> Option<PathBuf> {
    vci_service_config::find_library_path("iso22900", arch, library_name)
}

/// Library names configured with a `library_path` under
/// `config.apis.iso22900` in the shared `config.toml`, across both the
/// api-level and every arch-level `libs` table. Sorted and deduplicated.
pub fn list_configured_libraries() -> Vec<String> {
    vci_service_config::list_configured_libraries("iso22900")
}

/// Enumerates all D-PDU API libraries visible to this process: RDF-based
/// auto-discovery ([`enumerate_pdu_libraries`]) merged with `library_path`
/// entries configured under `config.apis.iso22900` in the shared
/// `config.toml` ([`list_configured_libraries`], including its `arch`-level
/// entries).
///
/// A library name present in both sources appears once, with `library_path`
/// set to the configured override (matching the priority
/// [`resolve_library_path`] applies at startup) and `source` set to
/// [`LibrarySource::Both`]. A name present in only one source keeps that
/// source's fields, with `source` set to [`LibrarySource::Registry`] or
/// [`LibrarySource::Config`] accordingly; config-only entries carry no
/// `description`/`supplier_name`/`module_description_path`/
/// `cable_description_path`, since the config file does not declare them.
///
/// RDF-based auto-discovery failures (missing RDF, XML parse errors, etc.)
/// are treated as "no RDF-discovered libraries" rather than propagated, so a
/// config-only library is always listed even if the RDF is broken or absent.
///
/// Sorted by `short_name`.
pub fn enumerate_libraries(mode: RegistryViewMode) -> Vec<PduLibraryInfo> {
    let mut libraries = match enumerate_pdu_libraries(mode) {
        Ok(libs) => libs,
        Err(e) => {
            warn!(error = %e, "RDF-based auto-discovery failed while enumerating libraries; falling back to config-only libraries");
            Vec::new()
        }
    };

    for (name, path) in vci_service_config::list_configured_library_paths("iso22900") {
        if let Some(existing) = libraries.iter_mut().find(|lib| lib.short_name == name) {
            existing.library_path = path;
            existing.source = LibrarySource::Both;
        } else {
            libraries.push(PduLibraryInfo {
                short_name: name,
                description: None,
                supplier_name: None,
                library_path: path,
                module_description_path: None,
                cable_description_path: None,
                arch: LibraryArch::get_native(),
                source: LibrarySource::Config,
            });
        }
    }

    libraries.sort_by(|a, b| a.short_name.cmp(&b.short_name));
    libraries
}

/// Resolves the filesystem path of the D-PDU API library named
/// `library_name`, applying the same priority `Iso22900Service::new()` used
/// to apply itself: a configured `library_path` override
/// (`find_library_path`) takes priority when present, falling back to
/// RDF-based auto-discovery (`find_pdu_libraries`) otherwise.
pub fn resolve_library_path(
    arch: Option<&str>,
    library_name: &str,
) -> Result<PathBuf, RegistryError> {
    if let Some(path) = find_library_path(arch, library_name) {
        return Ok(path);
    }
    Ok(find_pdu_libraries(&library_name)?.library_path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    fn enumerate_all_never_propagates_a_missing_rdf_as_not_found() {
        // RegistryViewMode::All folds over the per-view NotFound-skip policy
        // (ADR-223): a missing /etc/pdu_api_root.xml reached through the
        // Native fallback must be swallowed by the fold, never surfaced as
        // an error from All itself. This is the only path RegistryViewMode
        // ::All ever takes on this (non-Windows) CI host, so this is the
        // sole test exercising the real fold/try_fold machinery for it.
        if let Err(RegistryError::NotFound(_)) = enumerate_pdu_libraries(RegistryViewMode::All) {
            panic!("All mode must skip a per-view NotFound, not propagate it")
        }
    }

    #[test]
    fn find_on_registry_all_mode_returns_not_found_for_unknown_device() {
        match find_pdu_libraries_on_registry(
            &"__nonexistent_iso22900_device__",
            RegistryViewMode::All,
        ) {
            Err(RegistryError::NotFound(_)) => {}
            other => panic!("unexpected result: {other:?}"),
        }
    }
}

//! Shared resolution of a VCI name to its vendor library (7.1.1, 7.2; ADR-228 Decision item 4,
//! ADR-266).
//!
//! On Linux a VCI is registered by a TOML definition file in the fixed definition directory
//! (`vci_service_config::j2534_definition_dir()`); on Windows the registry is used. The
//! definition format, matching rules and error cases are in `docs/library-resolver.md`.
//!
//! This crate only resolves. The writability and signer checks of 7.2 are not performed here.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use tracing::{debug, warn};

mod definition;

pub use definition::{Definition, DefinitionError, PROTOCOL_KEYS};

/// Where a [`Resolved`] entry came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A definition file (the path of that file).
    Definition(PathBuf),
    /// The Windows registry.
    Registry,
}

/// A VCI name resolved to its library.
///
/// A registry hit carries only `name` and `library`; the other fields are empty or `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The resolved registration.
    pub definition: Definition,
    /// Where it was found.
    pub source: Source,
}

/// The result of reading a definition directory.
#[derive(Debug, Default)]
pub struct Definitions {
    /// Valid definitions with the file each came from, sorted by file path.
    pub valid: Vec<(PathBuf, Definition)>,
    /// Invalid `.toml` files with the reason, sorted by file path.
    pub invalid: Vec<(PathBuf, DefinitionError)>,
}

/// Errors from resolving a VCI name.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The definition directory exists but cannot be read.
    #[error("cannot read definition directory {}: {source}", path.display())]
    Io {
        /// The directory.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// No registration with this name exists.
    #[error("VCI '{name}' not found ({skipped_invalid} invalid definition file(s) were skipped)")]
    NotFound {
        /// The requested name.
        name: String,
        /// Number of invalid definition files skipped during the search; a typo in a
        /// definition shows up here.
        skipped_invalid: usize,
    },
    /// More than one valid definition has this name; none is picked.
    #[error(
        "VCI '{name}' is defined by more than one file: {}",
        files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(", ")
    )]
    Ambiguous {
        /// The requested name.
        name: String,
        /// The files defining it.
        files: Vec<PathBuf>,
    },
    /// The registry lookup failed.
    #[cfg(windows)]
    #[error("registry lookup failed: {0}")]
    Registry(j2534_0404_registry::RegistryError),
}

/// Reads every `*.toml` file directly in `dir`; other entries are ignored.
///
/// A missing directory yields no definitions. Any other failure to list it is an error.
/// Invalid files are returned in [`Definitions::invalid`] and logged at warn level.
pub fn read_definitions(dir: &Path) -> Result<Definitions, ResolveError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            debug!(dir = %dir.display(), "definition directory does not exist");
            return Ok(Definitions::default());
        }
        Err(source) => {
            return Err(ResolveError::Io {
                path: dir.to_owned(),
                source,
            });
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ResolveError::Io {
            path: dir.to_owned(),
            source,
        })?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "toml") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();

    let mut out = Definitions::default();
    for path in files {
        match fs::read_to_string(&path)
            .map_err(DefinitionError::from)
            .and_then(|text| Definition::parse(&text))
        {
            Ok(def) => out.valid.push((path, def)),
            Err(error) => {
                warn!(file = %path.display(), %error, "skipping invalid J2534 definition");
                out.invalid.push((path, error));
            }
        }
    }
    Ok(out)
}

/// Resolves `name` against the definitions in `dir`.
///
/// Exactly one valid definition must carry that `Name` (exact match). Invalid files are skipped;
/// their number is reported in [`ResolveError::NotFound`].
pub fn resolve_in_dir(dir: &Path, name: &str) -> Result<Resolved, ResolveError> {
    let Definitions { valid, invalid } = read_definitions(dir)?;
    let mut matches: Vec<_> = valid.into_iter().filter(|(_, d)| d.name == name).collect();
    match matches.len() {
        0 => Err(ResolveError::NotFound {
            name: name.to_owned(),
            skipped_invalid: invalid.len(),
        }),
        1 => {
            let (path, definition) = matches.remove(0);
            Ok(Resolved {
                definition,
                source: Source::Definition(path),
            })
        }
        _ => Err(ResolveError::Ambiguous {
            name: name.to_owned(),
            files: matches.into_iter().map(|(p, _)| p).collect(),
        }),
    }
}

/// Resolves `name` on this platform: the definition directory on non-Windows, the registry
/// (native view) on Windows.
pub fn resolve(name: &str) -> Result<Resolved, ResolveError> {
    #[cfg(windows)]
    {
        resolve_on_registry(name, j2534_0404_registry::RegistryViewMode::Native)
    }
    #[cfg(not(windows))]
    {
        resolve_in_dir(&vci_service_config::j2534_definition_dir(), name)
    }
}

/// Resolves `name` in the given registry view (Windows only).
#[cfg(windows)]
pub fn resolve_on_registry(
    name: &str,
    mode: j2534_0404_registry::RegistryViewMode,
) -> Result<Resolved, ResolveError> {
    use j2534_0404_registry::RegistryError;
    match j2534_0404_registry::find_j2534_device_on_registry(name, mode) {
        Ok(device) => Ok(Resolved {
            definition: Definition {
                name: device.device_name,
                vendor: None,
                library: device.library_path,
                config_application: None,
                protocols: Vec::new(),
                long_size: None,
                search_paths: Vec::new(),
            },
            source: Source::Registry,
        }),
        Err(RegistryError::NotFound(name)) => Err(ResolveError::NotFound {
            name,
            skipped_invalid: 0,
        }),
        Err(e) => Err(ResolveError::Registry(e)),
    }
}

#[cfg(test)]
mod tests;

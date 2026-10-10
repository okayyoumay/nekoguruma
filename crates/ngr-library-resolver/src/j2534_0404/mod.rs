//! J2534 v04.04: resolution of a VCI name to its vendor library (7.1.1; ADR-228 Decision item 4,
//! ADR-266).
//!
//! On Linux a VCI is registered by a TOML definition file in the fixed definition directory
//! (`vci_service_config::j2534_definition_dir()`); on Windows the registry is used. The
//! definition format, matching rules and error cases are in `docs/library-resolver.md`.
//!
//! `Name` is the VCI identifier callers resolve; on Windows it corresponds to the device's
//! registry key name. A leftover copy of a definition with the same `Name` makes resolution
//! ambiguous. On Windows `resolve` reads only the native registry view; use `resolve_on_registry`
//! per view when both are needed (mode `All` returns the first hit and does not detect a name
//! present in both views).

use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use tracing::{debug, warn};

mod definition;

pub use definition::{Definition, DefinitionError, MAX_DEFINITION_SIZE, PROTOCOL_KEYS};

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

impl Resolved {
    /// The files that named the library and so take part in the 7.2 writability check besides
    /// the library itself: the definition file for [`Source::Definition`], none for
    /// [`Source::Registry`] (HKLM is trusted by premise, 7.2). Pass the result to
    /// [`check_writability`](crate::check_writability).
    pub fn naming_files(&self) -> Vec<PathBuf> {
        match &self.source {
            Source::Definition(path) => vec![path.clone()],
            Source::Registry => Vec::new(),
        }
    }
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
    /// The definition directory is a symbolic link (or, on Windows, another reparse point such
    /// as a junction): it is refused, so the definitions read are those of the fixed directory
    /// itself (ADR-228 item 2, ADR-266).
    #[error("the definition directory {} is a link; it is refused", path.display())]
    LinkedDirectory {
        /// The directory.
        path: PathBuf,
    },
    /// No registration with this name exists.
    #[error("VCI '{name}' not found{}", skipped_note(*skipped_invalid))]
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
    Registry(#[source] j2534_0404_registry::RegistryError),
}

/// Reads every regular file directly in `dir` whose extension is exactly `toml`; other entries
/// are ignored. A file named just `.toml` has no extension and is ignored; hidden files such as
/// `.x.toml` are read. A `.toml` entry that is a symlink or directory is invalid
/// ([`DefinitionError::NotARegularFile`]). Files over [`MAX_DEFINITION_SIZE`] are invalid.
///
/// A missing directory yields no definitions. Any other failure to list it is an error.
/// Invalid files are returned in [`Definitions::invalid`] and logged at warn level.
pub fn read_definitions(dir: &Path) -> Result<Definitions, ResolveError> {
    // The directory itself must not be a link: `read_dir` would follow it, and the definitions
    // would come from wherever it points. Its parents are left to the 7.2 checks.
    match fs::symlink_metadata(dir) {
        Ok(metadata) if is_link(&metadata) => {
            return Err(ResolveError::LinkedDirectory {
                path: dir.to_owned(),
            });
        }
        Ok(_) => {}
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
    }
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
        if path.extension().is_some_and(|e| e == "toml") {
            // `DirEntry::file_type` does not follow symlinks.
            let regular = entry.file_type().is_ok_and(|t| t.is_file());
            files.push((path, regular));
        }
    }
    files.sort();

    let mut out = Definitions::default();
    for (path, regular) in files {
        let parsed = if regular {
            read_limited(&path).and_then(|text| Definition::parse(&text))
        } else {
            Err(DefinitionError::NotARegularFile)
        };
        match parsed {
            Ok(def) => out.valid.push((path, def)),
            Err(error) => {
                warn!(file = %path.display(), %error, "skipping invalid J2534 definition");
                out.invalid.push((path, error));
            }
        }
    }
    Ok(out)
}

fn skipped_note(n: usize) -> String {
    if n > 0 {
        format!(" ({n} invalid definition file(s) were skipped)")
    } else {
        String::new()
    }
}

/// Reads a definition file, refusing more than [`MAX_DEFINITION_SIZE`] bytes.
fn read_limited(path: &Path) -> Result<String, DefinitionError> {
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > MAX_DEFINITION_SIZE {
        return Err(DefinitionError::TooLarge);
    }
    // The file may grow after the metadata check, so the read is bounded as well.
    let mut text = String::new();
    file.take(MAX_DEFINITION_SIZE + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_DEFINITION_SIZE {
        return Err(DefinitionError::TooLarge);
    }
    Ok(text)
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

/// Whether `metadata` (not following links) is a symbolic link or, on Windows, any reparse
/// point (a junction, for one).
fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

/// Resolves `name` in the given registry view (Windows only).
#[cfg(windows)]
pub fn resolve_on_registry(
    name: &str,
    mode: j2534_0404_registry::RegistryViewMode,
) -> Result<Resolved, ResolveError> {
    use j2534_0404_registry::RegistryError;
    match j2534_0404_registry::find_j2534_device_on_registry(name, mode) {
        Ok(device) => Ok(resolved_from_registry(device)),
        Err(RegistryError::NotFound(name)) => Err(ResolveError::NotFound {
            name,
            skipped_invalid: 0,
        }),
        Err(e) => Err(ResolveError::Registry(e)),
    }
}

/// The [`Resolved`] of a registry hit: its key name and library; the registry lookup gives no
/// protocols, `LongSize` or search paths.
#[cfg(windows)]
fn resolved_from_registry(device: j2534_0404_registry::J2534DeviceInfo) -> Resolved {
    Resolved {
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
    }
}

#[cfg(test)]
mod tests;

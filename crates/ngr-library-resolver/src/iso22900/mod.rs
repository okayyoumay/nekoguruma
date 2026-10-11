//! ISO 22900 (D-PDU API): resolution of an implementation name to its vendor library (7.1;
//! ISO 22900-2:2022 clause 8.7 and Annex F; ADR-228 Decision item 4).
//!
//! Chain: the root description file lists one `MVCI_PDU_API` entry per installed
//! implementation; the entry itself names the API library, the module description file (MDF) and
//! the cable description file (CDF), each as a `file:` URI. The root file is found through the
//! registry value `Root File` under `HKLM\SOFTWARE\D-PDU API` on Windows and at
//! `vci_service_config::pdu_api_root_file()` elsewhere. The MDF and CDF are not parsed here; their
//! paths are reported. Matching rules and error cases are in `docs/library-resolver.md`.
//!
//! The name a caller resolves is the entry's `SHORT_NAME` (trimmed), matched exactly and
//! case-sensitively.

use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use tracing::warn;

/// Largest root description file accepted, in bytes.
pub const MAX_ROOT_FILE_SIZE: u64 = 1024 * 1024;

/// One `MVCI_PDU_API` entry of the root description file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Implementation {
    /// The entry's `SHORT_NAME` (trimmed); the name a caller resolves.
    pub short_name: String,
    /// `DESCRIPTION`, display only.
    pub description: Option<String>,
    /// `SUPPLIER_NAME`, display only.
    pub supplier_name: Option<String>,
    /// The API library (`LIBRARY_FILE`), an absolute path.
    pub library_file: PathBuf,
    /// The module description file (`MODULE_DESCRIPTION_FILE`), if the entry names one.
    pub module_description_file: Option<PathBuf>,
    /// The cable description file (`CABLE_DESCRIPTION_FILE`), if the entry names one.
    pub cable_description_file: Option<PathBuf>,
}

/// An implementation resolved from a root description file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The matching entry.
    pub implementation: Implementation,
    /// The root description file it came from.
    pub root_file: PathBuf,
}

impl Resolved {
    /// The files that named the library and so take part in the 7.2 writability check besides
    /// the library itself: the root file, then the MDF and the CDF when the entry names them.
    /// Pass the result to [`check_writability`](crate::check_writability) together with
    /// [`Implementation::library_file`].
    pub fn naming_files(&self) -> Vec<PathBuf> {
        let mut files = vec![self.root_file.clone()];
        files.extend(self.implementation.module_description_file.clone());
        files.extend(self.implementation.cable_description_file.clone());
        files
    }
}

/// Why an entry of the root file cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntryError {
    /// The entry has no `SHORT_NAME`, so no caller could resolve it.
    #[error("the entry has no SHORT_NAME")]
    MissingShortName,
    /// The entry names no `LIBRARY_FILE` (or the element has no `URI` attribute).
    #[error("the entry names no LIBRARY_FILE")]
    MissingLibrary,
    /// A file element holds something that is not a usable `file:` URI.
    #[error("{element} is not a valid file URI: {uri:?}")]
    InvalidUri {
        /// The element name.
        element: &'static str,
        /// The attribute value.
        uri: String,
    },
    /// The library URI does not convert to an absolute path (on Windows, no drive letter or
    /// host).
    #[error("LIBRARY_FILE is not an absolute path: {}", path.display())]
    RelativeLibrary {
        /// The converted path.
        path: PathBuf,
    },
}

/// An entry that could not be used, with its `SHORT_NAME` if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidEntry {
    /// The entry's trimmed `SHORT_NAME`.
    pub short_name: Option<String>,
    /// The reason.
    pub error: EntryError,
}

/// The parsed content of a root description file.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RootFile {
    /// Usable entries, in document order.
    pub implementations: Vec<Implementation>,
    /// Entries that are unusable, in document order.
    pub invalid: Vec<InvalidEntry>,
}

/// Errors from resolving an implementation name.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// There is no root description file: no registry value on Windows, or the file does not
    /// exist. Distinct from a name that is not in an existing root file.
    #[error("no D-PDU API root file{}", located(path.as_deref()))]
    NoRootFile {
        /// The expected location, when known (always on non-Windows).
        path: Option<PathBuf>,
    },
    /// The root file cannot be read.
    #[error("cannot read D-PDU API root file {}: {source}", path.display())]
    Io {
        /// The root file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The root file is larger than [`MAX_ROOT_FILE_SIZE`].
    #[error(
        "D-PDU API root file {} is larger than {MAX_ROOT_FILE_SIZE} bytes",
        path.display()
    )]
    TooLarge {
        /// The root file.
        path: PathBuf,
    },
    /// The root file is not well-formed XML (or not UTF-8).
    #[error("invalid XML in D-PDU API root file {}: {source}", path.display())]
    Xml {
        /// The root file; `<memory>` for [`parse_root_file`].
        path: PathBuf,
        /// The parser error.
        source: roxmltree::Error,
    },
    /// No entry has this `SHORT_NAME`.
    #[error("D-PDU API '{name}' not found{}", skipped_note(*skipped_invalid))]
    NotFound {
        /// The requested name.
        name: String,
        /// Number of unusable entries skipped during the search; a typo in an entry shows up
        /// here.
        skipped_invalid: usize,
    },
    /// More than one entry has this `SHORT_NAME`; none is picked.
    #[error("D-PDU API '{name}' is listed {count} times in {}", root_file.display())]
    Ambiguous {
        /// The requested name.
        name: String,
        /// The root file.
        root_file: PathBuf,
        /// Number of entries with this name.
        count: usize,
    },
    /// The entry with this name is unusable (no library, an invalid URI, a relative library);
    /// it is refused.
    #[error("D-PDU API '{name}' in {} is unusable: {source}", root_file.display())]
    InvalidEntry {
        /// The requested name.
        name: String,
        /// The root file.
        root_file: PathBuf,
        /// The reason.
        #[source]
        source: EntryError,
    },
    /// The registry lookup of the root file failed.
    #[cfg(windows)]
    #[error("registry lookup of the D-PDU API root file failed: {0}")]
    Registry(#[source] io::Error),
}

fn located(path: Option<&Path>) -> String {
    path.map(|p| format!(" at {}", p.display()))
        .unwrap_or_else(|| " is registered".to_owned())
}

fn skipped_note(n: usize) -> String {
    if n > 0 {
        format!(" ({n} unusable entry(ies) were skipped)")
    } else {
        String::new()
    }
}

/// Parses the contents of a root description file. Entries that are unusable are returned in
/// [`RootFile::invalid`] rather than failing the whole file. XML that does not parse is
/// [`ResolveError::Xml`] with the path `<memory>`.
pub fn parse_root_file(xml: &str) -> Result<RootFile, ResolveError> {
    parse_document(xml, Path::new("<memory>"))
}

fn parse_document(xml: &str, path: &Path) -> Result<RootFile, ResolveError> {
    let doc = roxmltree::Document::parse(xml).map_err(|source| ResolveError::Xml {
        path: path.to_owned(),
        source,
    })?;
    let mut out = RootFile::default();
    for api in doc.descendants().filter(|n| n.has_tag_name("MVCI_PDU_API")) {
        let child = |name: &str| api.children().find(|n| n.has_tag_name(name));
        let text = |name: &str| {
            child(name)
                .and_then(|n| n.text())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let uri = |element: &'static str| {
            child(element)
                .and_then(|n| n.attribute("URI"))
                .map(|uri| {
                    uri_to_path(uri).ok_or_else(|| EntryError::InvalidUri {
                        element,
                        uri: uri.to_owned(),
                    })
                })
                .transpose()
        };
        let short_name = text("SHORT_NAME");
        let entry = (|| {
            let short_name = short_name.clone().ok_or(EntryError::MissingShortName)?;
            let library_file = uri("LIBRARY_FILE")?.ok_or(EntryError::MissingLibrary)?;
            if !library_file.is_absolute() {
                return Err(EntryError::RelativeLibrary { path: library_file });
            }
            Ok(Implementation {
                short_name,
                description: text("DESCRIPTION"),
                supplier_name: text("SUPPLIER_NAME"),
                library_file,
                module_description_file: uri("MODULE_DESCRIPTION_FILE")?,
                cable_description_file: uri("CABLE_DESCRIPTION_FILE")?,
            })
        })();
        match entry {
            Ok(implementation) => out.implementations.push(implementation),
            Err(error) => out.invalid.push(InvalidEntry { short_name, error }),
        }
    }
    Ok(out)
}

/// Reads and parses a root description file, refusing more than [`MAX_ROOT_FILE_SIZE`] bytes.
pub fn read_root_file(path: &Path) -> Result<RootFile, ResolveError> {
    let io_err = |source| ResolveError::Io {
        path: path.to_owned(),
        source,
    };
    let file = fs::File::open(path).map_err(io_err)?;
    if file.metadata().map_err(io_err)?.len() > MAX_ROOT_FILE_SIZE {
        return Err(ResolveError::TooLarge {
            path: path.to_owned(),
        });
    }
    // The file may grow after the metadata check, so the read is bounded as well.
    let mut text = String::new();
    file.take(MAX_ROOT_FILE_SIZE + 1)
        .read_to_string(&mut text)
        .map_err(io_err)?;
    if text.len() as u64 > MAX_ROOT_FILE_SIZE {
        return Err(ResolveError::TooLarge {
            path: path.to_owned(),
        });
    }
    parse_document(&text, path)
}

/// Resolves `name` against the root description file `root_file`.
///
/// Exactly one entry must carry that `SHORT_NAME` (exact, case-sensitive match against the
/// trimmed element text), and it must be usable. Unusable entries of other names are skipped,
/// logged at warn level, and counted in [`ResolveError::NotFound`]. A root file that does not
/// exist is [`ResolveError::NoRootFile`].
pub fn resolve_in_root_file(root_file: &Path, name: &str) -> Result<Resolved, ResolveError> {
    let RootFile {
        implementations,
        invalid,
    } = match read_root_file(root_file) {
        Ok(parsed) => parsed,
        Err(ResolveError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ResolveError::NoRootFile {
                path: Some(root_file.to_owned()),
            });
        }
        Err(e) => return Err(e),
    };
    let mut matches: Vec<_> = implementations
        .into_iter()
        .filter(|i| i.short_name == name)
        .collect();
    let (named, other): (Vec<_>, Vec<_>) = invalid
        .into_iter()
        .partition(|e| e.short_name.as_deref() == Some(name));
    for entry in &other {
        warn!(
            file = %root_file.display(),
            short_name = entry.short_name.as_deref().unwrap_or(""),
            error = %entry.error,
            "skipping unusable D-PDU API entry"
        );
    }
    let count = matches.len() + named.len();
    if count > 1 {
        return Err(ResolveError::Ambiguous {
            name: name.to_owned(),
            root_file: root_file.to_owned(),
            count,
        });
    }
    if let Some(entry) = named.into_iter().next() {
        return Err(ResolveError::InvalidEntry {
            name: name.to_owned(),
            root_file: root_file.to_owned(),
            source: entry.error,
        });
    }
    match matches.pop() {
        Some(implementation) => Ok(Resolved {
            implementation,
            root_file: root_file.to_owned(),
        }),
        None => Err(ResolveError::NotFound {
            name: name.to_owned(),
            skipped_invalid: other.len(),
        }),
    }
}

/// Location of the root description file: the registry value `Root File` under
/// `HKLM\SOFTWARE\D-PDU API` on Windows (native view; `None` when the key, the value or its
/// content is absent), `vci_service_config::pdu_api_root_file()` elsewhere (always `Some`; the
/// file may not exist).
pub fn root_file_path() -> Result<Option<PathBuf>, ResolveError> {
    #[cfg(windows)]
    {
        use winreg::{
            RegKey,
            enums::{HKEY_LOCAL_MACHINE, KEY_READ},
        };
        let key = match RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags(r"SOFTWARE\D-PDU API", KEY_READ)
        {
            Ok(key) => key,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                tracing::debug!("no D-PDU API registry key");
                return Ok(None);
            }
            Err(e) => return Err(ResolveError::Registry(e)),
        };
        root_file_from_key(&key)
    }
    #[cfg(not(windows))]
    {
        Ok(Some(vci_service_config::pdu_api_root_file()))
    }
}

/// Reads the `Root File` value (trimmed) of an opened `D-PDU API` registry key.
#[cfg(windows)]
fn root_file_from_key(key: &winreg::RegKey) -> Result<Option<PathBuf>, ResolveError> {
    match key.get_value::<String, _>("Root File") {
        Ok(path) => {
            let path = path.trim();
            Ok((!path.is_empty()).then(|| PathBuf::from(path)))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            tracing::debug!("no Root File value under the D-PDU API registry key");
            Ok(None)
        }
        Err(e) => Err(ResolveError::Registry(e)),
    }
}

/// Resolves `name` on this platform: locates the root file ([`root_file_path`]) and resolves in
/// it.
pub fn resolve(name: &str) -> Result<Resolved, ResolveError> {
    match root_file_path()? {
        Some(root_file) => resolve_in_root_file(&root_file, name),
        None => Err(ResolveError::NoRootFile { path: None }),
    }
}

/// Converts a `file:` URI to a path (RFC 8089 forms seen in root files): `file:/c:/dir/x.dll`,
/// `file:///c:/dir/x.dll`, `file:///opt/x/libx.so` and `file://localhost/...`.
/// Percent-encoding is decoded. On Windows a remote host becomes a UNC path; elsewhere a remote
/// host or a drive letter is rejected. `None` if the text is not such a URI.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let uri = uri.trim();
    let rest = uri
        .get(..5)
        .filter(|scheme| scheme.eq_ignore_ascii_case("file:"))
        .map(|_| &uri[5..])?;
    let (host, path) = match rest.strip_prefix("//") {
        Some(authority) => {
            let i = authority.find('/')?;
            (&authority[..i], &authority[i..])
        }
        None => ("", rest),
    };
    if !path.starts_with('/') {
        return None;
    }
    let path = percent_decode(path)?;
    if path.contains('\0') {
        return None;
    }
    let host = if host.eq_ignore_ascii_case("localhost") {
        ""
    } else {
        host
    };

    // "/c:/dir" -> "c:/dir"
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 3 && bytes[1].is_ascii_alphabetic() && bytes[2] == b':';
    if cfg!(windows) {
        let path = if drive { &path[1..] } else { &path[..] };
        let path = path.replace('/', "\\");
        Some(if host.is_empty() {
            path.into()
        } else {
            format!("\\\\{host}{path}").into()
        })
    } else if host.is_empty() && !drive {
        Some(path.into())
    } else {
        None
    }
}

fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests;

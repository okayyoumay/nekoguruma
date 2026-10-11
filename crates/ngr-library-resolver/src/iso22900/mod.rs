//! ISO 22900 (D-PDU API): resolution of an implementation name to its vendor library (7.1;
//! ISO 22900-2:2022 clause 8.7 and Annex F; ADR-228 Decision item 4; ADR-277).
//!
//! Chain: the root description file lists one `MVCI_PDU_API` entry per installed
//! implementation; the entry itself names the API library, the module description file (MDF) and
//! the cable description file (CDF), each as a `file:` URI. The root file is found through the
//! registry value `Root File` under `HKLM\SOFTWARE\D-PDU API` on Windows, read in an explicit
//! [`RegistryView`], and at `vci_service_config::pdu_api_root_file()` elsewhere. The MDF and CDF
//! are not parsed here and are not followed to find the library; their paths are reported so the
//! 7.2 check can cover them. Matching rules and error cases are in `docs/library-resolver.md`.
//!
//! The name a caller resolves is the entry's `SHORT_NAME` (trimmed), matched exactly and
//! case-sensitively.

use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use tracing::warn;

use crate::RegistryView;

/// Largest root description file accepted, in bytes.
pub const MAX_ROOT_FILE_SIZE: u64 = 1024 * 1024;

/// The document element of a root description file.
const ROOT_ELEMENT: &str = "MVCI_PDU_API_ROOT";

/// The child elements of an entry that may appear at most once.
const KNOWN_CHILDREN: [&str; 6] = [
    "SHORT_NAME",
    "DESCRIPTION",
    "SUPPLIER_NAME",
    "LIBRARY_FILE",
    "MODULE_DESCRIPTION_FILE",
    "CABLE_DESCRIPTION_FILE",
];

/// One `MVCI_PDU_API` entry of the root description file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Implementation {
    /// The entry's `SHORT_NAME` (trimmed); the name a caller resolves.
    pub short_name: String,
    /// `DESCRIPTION`, display only.
    pub description: Option<String>,
    /// `SUPPLIER_NAME`, display only.
    pub supplier_name: Option<String>,
    /// The API library (`LIBRARY_FILE`), an absolute local path.
    pub library_file: PathBuf,
    /// The module description file (`MODULE_DESCRIPTION_FILE`), if the entry names one; an
    /// absolute local path.
    pub module_description_file: Option<PathBuf>,
    /// The cable description file (`CABLE_DESCRIPTION_FILE`), if the entry names one; an
    /// absolute local path.
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
    /// The `SHORT_NAME` holds an element, so its text is not a plain name.
    #[error("SHORT_NAME contains a nested element")]
    NestedShortName,
    /// The entry names no `LIBRARY_FILE` (or the element has no `URI` attribute).
    #[error("the entry names no LIBRARY_FILE")]
    MissingLibrary,
    /// One of the entry's child elements appears more than once.
    #[error("{element} appears more than once in the entry")]
    Duplicate {
        /// The element name.
        element: &'static str,
    },
    /// A file element holds something that is not a usable `file:` URI: another scheme, a
    /// malformed or unencoded reserved character, a `..` component, a NUL, an empty path
    /// (`file:///`), or (off Windows) a drive-letter path. On Windows a path that is not in
    /// drive-letter form is [`RelativePath`](Self::RelativePath).
    #[error("{element} is not a valid file URI: {uri:?}")]
    InvalidUri {
        /// The element name.
        element: &'static str,
        /// The attribute value.
        uri: String,
    },
    /// A file element names a non-local file: a host other than `localhost`, or a path that
    /// starts with `//` or `/\` (a share). The 7.2 writability premise cannot be established for it.
    #[error("{element} names a remote file: {uri:?}")]
    RemoteHost {
        /// The element name.
        element: &'static str,
        /// The attribute value.
        uri: String,
    },
    /// A file element does not convert to an absolute path (on Windows, no drive letter).
    #[error("{element} is not an absolute path: {uri:?}")]
    RelativePath {
        /// The element name.
        element: &'static str,
        /// The attribute value.
        uri: String,
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
    /// The root file is neither UTF-8 nor UTF-16 with a byte order mark (UTF-32, with or without
    /// a mark, and UTF-16 without one are not accepted).
    #[error(
        "D-PDU API root file {} is not UTF-8 or byte-order-marked UTF-16",
        path.display()
    )]
    Encoding {
        /// The root file.
        path: PathBuf,
    },
    /// The root file is not well-formed XML.
    #[error("invalid XML in D-PDU API root file {}: {source}", path.display())]
    Xml {
        /// The root file; `<memory>` for [`parse_root_file`].
        path: PathBuf,
        /// The parser error.
        source: roxmltree::Error,
    },
    /// The document element is not `MVCI_PDU_API_ROOT`.
    #[error(
        "D-PDU API root file {} is not a root description file (document element {element:?})",
        path.display()
    )]
    NotARootFile {
        /// The root file; `<memory>` for [`parse_root_file`].
        path: PathBuf,
        /// The local name of the document element found.
        element: String,
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
    /// The entry with this name is unusable (no library, an invalid, remote or relative path, a
    /// duplicated child); it is refused.
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
    /// The registry value `Root File` cannot be used: not valid UTF-16, a NUL inside, a `%NAME%`
    /// reference that is not one of the supported folders (or whose registry value is missing),
    /// an unpaired `%`, or a result that is not an absolute drive-letter path.
    #[error("unusable D-PDU API registry value Root File: {reason}")]
    RootFileValue {
        /// What is wrong with the value.
        reason: String,
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
/// [`ResolveError::Xml`], and a document element other than `MVCI_PDU_API_ROOT` is
/// [`ResolveError::NotARootFile`], both with the path `<memory>`.
pub fn parse_root_file(xml: &str) -> Result<RootFile, ResolveError> {
    parse_document(xml, Path::new("<memory>"))
}

/// The text of an element: its text and CDATA children joined, so a comment inside does not cut
/// it short. Nested elements are skipped.
fn element_text(node: roxmltree::Node<'_, '_>) -> String {
    node.children()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
        .collect()
}

fn is_named(node: &roxmltree::Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}

fn parse_document(xml: &str, path: &Path) -> Result<RootFile, ResolveError> {
    let doc = roxmltree::Document::parse(xml).map_err(|source| ResolveError::Xml {
        path: path.to_owned(),
        source,
    })?;
    let document = doc.root_element();
    if !is_named(&document, ROOT_ELEMENT) {
        return Err(ResolveError::NotARootFile {
            path: path.to_owned(),
            element: document.tag_name().name().to_owned(),
        });
    }
    let mut out = RootFile::default();
    for api in document.children().filter(|n| is_named(n, "MVCI_PDU_API")) {
        let child = |name: &str| api.children().find(|n| is_named(n, name));
        let text = |name: &str| {
            child(name)
                .map(|n| element_text(n).trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let uri = |element: &'static str| {
            child(element)
                .and_then(|n| {
                    // `Node::attribute("URI")` would also match a prefixed `p:URI`.
                    n.attributes()
                        .find(|a| a.name() == "URI" && a.namespace().is_none())
                        .map(|a| a.value())
                })
                .map(|uri| {
                    uri_to_path(uri).map_err(|fault| match fault {
                        UriFault::Invalid => EntryError::InvalidUri {
                            element,
                            uri: uri.to_owned(),
                        },
                        UriFault::Remote => EntryError::RemoteHost {
                            element,
                            uri: uri.to_owned(),
                        },
                        UriFault::Relative => EntryError::RelativePath {
                            element,
                            uri: uri.to_owned(),
                        },
                    })
                })
                .transpose()
        };
        let short_name = text("SHORT_NAME");
        let entry = (|| {
            for element in KNOWN_CHILDREN {
                if api.children().filter(|n| is_named(n, element)).count() > 1 {
                    return Err(EntryError::Duplicate { element });
                }
            }
            if child("SHORT_NAME").is_some_and(|n| n.children().any(|c| c.is_element())) {
                return Err(EntryError::NestedShortName);
            }
            let short_name = short_name.clone().ok_or(EntryError::MissingShortName)?;
            let library_file = uri("LIBRARY_FILE")?.ok_or(EntryError::MissingLibrary)?;
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

/// Decodes the bytes of a root file: UTF-8 (a byte order mark is tolerated) or UTF-16 with a
/// little- or big-endian byte order mark. `None` for anything else, including UTF-32 with a mark
/// and UTF-16 without one (which is not UTF-8, or fails as XML).
fn decode_root_bytes(bytes: &[u8]) -> Option<String> {
    // The UTF-32LE mark starts like the UTF-16LE one, so it is tested first.
    if bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) || bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF])
    {
        return None;
    }
    let text = if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        utf16(rest, u16::from_le_bytes)?
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        utf16(rest, u16::from_be_bytes)?
    } else {
        String::from_utf8(bytes.to_vec()).ok()?
    };
    Some(match text.strip_prefix('\u{FEFF}') {
        Some(rest) => rest.to_owned(),
        None => text,
    })
}

fn utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Option<String> {
    if bytes.len() & 1 == 1 {
        return None;
    }
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| unit([c[0], c[1]])).collect();
    String::from_utf16(&units).ok()
}

/// Reads and parses a root description file, refusing more than [`MAX_ROOT_FILE_SIZE`] bytes.
/// The file must be UTF-8 or byte-order-marked UTF-16 ([`ResolveError::Encoding`] otherwise); a
/// declared XML encoding is not honoured.
pub fn read_root_file(path: &Path) -> Result<RootFile, ResolveError> {
    let io_err = |source| ResolveError::Io {
        path: path.to_owned(),
        source,
    };
    let too_large = || ResolveError::TooLarge {
        path: path.to_owned(),
    };
    let file = fs::File::open(path).map_err(io_err)?;
    if file.metadata().map_err(io_err)?.len() > MAX_ROOT_FILE_SIZE {
        return Err(too_large());
    }
    // The file may grow after the metadata check, so the read is bounded as well.
    let mut bytes = Vec::new();
    file.take(MAX_ROOT_FILE_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(io_err)?;
    if bytes.len() as u64 > MAX_ROOT_FILE_SIZE {
        return Err(too_large());
    }
    let text = decode_root_bytes(&bytes).ok_or_else(|| ResolveError::Encoding {
        path: path.to_owned(),
    })?;
    parse_document(&text, path)
}

/// Resolves `name` against the root description file `root_file`.
///
/// Exactly one entry must carry that `SHORT_NAME` (exact, case-sensitive match against the
/// trimmed element text), and it must be usable. Unusable entries of other names are skipped,
/// logged at warn level, and counted in [`ResolveError::NotFound`]. A root file that does not
/// exist is [`ResolveError::NoRootFile`]. An entry without an MDF or CDF is usable, but logged at
/// warn level, since the schema (Annex F) requires them.
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
        Some(implementation) => {
            for (element, file) in [
                (
                    "MODULE_DESCRIPTION_FILE",
                    &implementation.module_description_file,
                ),
                (
                    "CABLE_DESCRIPTION_FILE",
                    &implementation.cable_description_file,
                ),
            ] {
                if file.is_none() {
                    warn!(
                        file = %root_file.display(),
                        short_name = name,
                        element,
                        "D-PDU API entry lacks an element the root file schema requires"
                    );
                }
            }
            Ok(Resolved {
                implementation,
                root_file: root_file.to_owned(),
            })
        }
        None => Err(ResolveError::NotFound {
            name: name.to_owned(),
            skipped_invalid: other.len(),
        }),
    }
}

/// Location of the root description file: the registry value `Root File` under
/// `HKLM\SOFTWARE\D-PDU API` in registry view `view` on Windows (`None` when the key, the value
/// or its content is absent; a `REG_EXPAND_SZ` value is expanded from the same view's HKLM folder
/// values, never from the process environment, 7.2; the result must be an absolute drive-letter
/// path), `vci_service_config::pdu_api_root_file()` elsewhere (always `Some`; the file may not
/// exist; `view` is ignored).
pub fn root_file_path(view: RegistryView) -> Result<Option<PathBuf>, ResolveError> {
    #[cfg(windows)]
    {
        use winreg::{
            RegKey,
            enums::{HKEY_LOCAL_MACHINE, KEY_READ},
        };
        let key = match RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags(r"SOFTWARE\D-PDU API", KEY_READ | view.flag())
        {
            Ok(key) => key,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                tracing::debug!("no D-PDU API registry key");
                return Ok(None);
            }
            Err(e) => return Err(ResolveError::Registry(e)),
        };
        // The folder values are read in the same view as `Root File`: in the 32-bit view the
        // `CurrentVersion` key is redirected, so `ProgramFilesDir` there is the x86 folder.
        let lookup = |folder: KnownFolder| {
            let (subkey, value) = folder.registry_location();
            let key = RegKey::predef(HKEY_LOCAL_MACHINE)
                .open_subkey_with_flags(subkey, KEY_READ | view.flag())
                .ok()?;
            registry_folder(&key, value)
        };
        root_file_from_key(&key, &lookup)
    }
    #[cfg(not(windows))]
    {
        let _ = view;
        Ok(Some(vci_service_config::pdu_api_root_file()))
    }
}

/// Reads the `Root File` value of an opened `D-PDU API` registry key. Only a `REG_EXPAND_SZ`
/// value has its `%NAME%` references expanded (from `lookup`); a `REG_SZ` value is taken
/// literally.
#[cfg(windows)]
fn root_file_from_key(
    key: &winreg::RegKey,
    lookup: &dyn Fn(KnownFolder) -> Option<String>,
) -> Result<Option<PathBuf>, ResolveError> {
    use winreg::enums::{REG_EXPAND_SZ, REG_SZ};
    let value = match key.get_raw_value("Root File") {
        Ok(value) => value,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            tracing::debug!("no Root File value under the D-PDU API registry key");
            return Ok(None);
        }
        Err(e) => return Err(ResolveError::Registry(e)),
    };
    let expand = match value.vtype {
        REG_SZ => false,
        REG_EXPAND_SZ => true,
        other => {
            return Err(ResolveError::Registry(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Root File has registry type {other:?}, not a string"),
            )));
        }
    };
    root_file_from_value(&value.bytes, expand, lookup)
}

/// The string value `value` of an opened folder key, if it is a `REG_SZ` (not an expandable
/// string, so no expansion happens inside an expansion) and not empty.
#[cfg(windows)]
fn registry_folder(key: &winreg::RegKey, value: &str) -> Option<String> {
    use winreg::enums::REG_SZ;
    let raw = key.get_raw_value(value).ok()?;
    if !matches!(raw.vtype, REG_SZ) {
        return None;
    }
    let text = utf16(&raw.bytes, u16::from_le_bytes)?;
    let text = text.trim_end_matches('\0');
    (!text.is_empty()).then(|| text.to_owned())
}

/// A bad `Root File` value.
#[cfg(any(windows, test))]
fn bad_value(reason: impl Into<String>) -> ResolveError {
    ResolveError::RootFileValue {
        reason: reason.into(),
    }
}

/// The path held by the raw bytes of a registry string value (UTF-16LE, trailing NULs stripped,
/// trimmed), expanded when `expand` is set. `None` if blank. A non-blank result must be an
/// absolute drive-letter path.
#[cfg(any(windows, test))]
fn root_file_from_value(
    bytes: &[u8],
    expand: bool,
    lookup: &dyn Fn(KnownFolder) -> Option<String>,
) -> Result<Option<PathBuf>, ResolveError> {
    let text = utf16(bytes, u16::from_le_bytes)
        .ok_or_else(|| bad_value("Root File is not valid UTF-16"))?;
    let text = text.trim_end_matches('\0');
    if text.contains('\0') {
        return Err(bad_value("Root File contains a NUL character"));
    }
    let text = if expand {
        expand_root_file_value(text, lookup)?
    } else {
        text.to_owned()
    };
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if !is_drive_letter_path(text) {
        return Err(bad_value(format!(
            "Root File {text:?} is not an absolute drive-letter path"
        )));
    }
    Ok(Some(PathBuf::from(text)))
}

/// `X:\...` or `X:/...`. A relative path, a drive-relative path (`X:dir`), a UNC path and a
/// `\\?\` path all fail.
#[cfg(any(windows, test))]
fn is_drive_letter_path(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// The folders a `REG_EXPAND_SZ` `Root File` may refer to. Each is read from HKLM in the view of
/// the `Root File` value, never from the process environment (7.2).
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KnownFolder {
    /// `%ProgramFiles%`.
    ProgramFiles,
    /// `%CommonProgramFiles%`.
    CommonProgramFiles,
    /// `%ProgramFiles(x86)%`.
    ProgramFilesX86,
    /// `%CommonProgramFiles(x86)%`.
    CommonProgramFilesX86,
    /// `%ProgramW6432%`.
    ProgramW6432,
    /// `%CommonProgramW6432%`.
    CommonProgramW6432,
    /// `%SystemRoot%` and `%windir%`.
    SystemRoot,
}

#[cfg(any(windows, test))]
impl KnownFolder {
    /// The folder a `%NAME%` stands for (matched case-insensitively), if it is one of the fixed
    /// names.
    fn from_name(name: &str) -> Option<Self> {
        const NAMES: [(&str, KnownFolder); 8] = [
            ("ProgramFiles", KnownFolder::ProgramFiles),
            ("CommonProgramFiles", KnownFolder::CommonProgramFiles),
            ("ProgramFiles(x86)", KnownFolder::ProgramFilesX86),
            (
                "CommonProgramFiles(x86)",
                KnownFolder::CommonProgramFilesX86,
            ),
            ("ProgramW6432", KnownFolder::ProgramW6432),
            ("CommonProgramW6432", KnownFolder::CommonProgramW6432),
            ("SystemRoot", KnownFolder::SystemRoot),
            ("windir", KnownFolder::SystemRoot),
        ];
        NAMES
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|&(_, folder)| folder)
    }

    /// The HKLM subkey and value name that hold this folder.
    fn registry_location(self) -> (&'static str, &'static str) {
        const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion";
        const NT_CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
        match self {
            Self::ProgramFiles => (CURRENT_VERSION, "ProgramFilesDir"),
            Self::CommonProgramFiles => (CURRENT_VERSION, "CommonFilesDir"),
            Self::ProgramFilesX86 => (CURRENT_VERSION, "ProgramFilesDir (x86)"),
            Self::CommonProgramFilesX86 => (CURRENT_VERSION, "CommonFilesDir (x86)"),
            Self::ProgramW6432 => (CURRENT_VERSION, "ProgramW6432Dir"),
            Self::CommonProgramW6432 => (CURRENT_VERSION, "CommonW6432Dir"),
            Self::SystemRoot => (NT_CURRENT_VERSION, "SystemRoot"),
        }
    }
}

/// Expands the `%NAME%` references of a `REG_EXPAND_SZ` `Root File` value. Only the names of
/// [`KnownFolder`] are expanded, from `lookup`; the process environment is not consulted (7.2).
/// Any other name, an empty `%%`, an unpaired `%` and a known name `lookup` has no value for are
/// [`ResolveError::RootFileValue`]. Expanded text is not scanned again.
#[cfg(any(windows, test))]
fn expand_root_file_value(
    text: &str,
    lookup: impl Fn(KnownFolder) -> Option<String>,
) -> Result<String, ResolveError> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let j = after
            .find('%')
            .ok_or_else(|| bad_value("Root File has an unpaired '%'"))?;
        let name = &after[..j];
        if name.is_empty() {
            return Err(bad_value("Root File has an empty '%%'"));
        }
        let folder = KnownFolder::from_name(name)
            .ok_or_else(|| bad_value(format!("Root File refers to unsupported %{name}%")))?;
        let value = lookup(folder)
            .ok_or_else(|| bad_value(format!("no registry value for %{name}% in this view")))?;
        out.push_str(&value);
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Resolves `name` in registry view `view`: locates the root file ([`root_file_path`]) and
/// resolves in it.
pub fn resolve_in_view(name: &str, view: RegistryView) -> Result<Resolved, ResolveError> {
    match root_file_path(view)? {
        Some(root_file) => resolve_in_root_file(&root_file, name),
        None => Err(ResolveError::NoRootFile { path: None }),
    }
}

/// Resolves `name` on this platform, in the registry view of this build
/// ([`RegistryView::native`]).
pub fn resolve(name: &str) -> Result<Resolved, ResolveError> {
    resolve_in_view(name, RegistryView::native())
}

/// Why a `file:` URI is not a usable path.
#[derive(Debug, PartialEq, Eq)]
enum UriFault {
    /// Not a `file:` URI, or malformed or unsafe.
    Invalid,
    /// Names a host other than `localhost`, or a share.
    Remote,
    /// Does not convert to an absolute path.
    Relative,
}

/// Converts a `file:` URI to an absolute local path (RFC 8089 forms seen in root files):
/// `file:/dir/x`, `file:///dir/x` and `file://localhost/dir/x`; on Windows the path must be in
/// drive-letter form (`file:///c:/dir/x.dll`). Percent-encoding is decoded (two hex digits
/// after each `%`). ASCII whitespace around the URI is trimmed.
///
/// Refused: another scheme; a host other than `localhost` (which includes `file://c:/dir`); a
/// path starting with `//` or `/\` before or after decoding (a share); a raw `?` or `#` (a query
/// or fragment, or an unencoded character; `%3F` and `%23` decode normally); a `..` component or
/// a NUL after decoding; an empty path (`file:///`); on Windows a path without a drive letter; elsewhere a path that looks
/// like a drive letter.
fn uri_to_path(uri: &str) -> Result<PathBuf, UriFault> {
    // Only ASCII whitespace is trimmed; any other character stays part of the value.
    let uri = uri.trim_matches(|c: char| c.is_ascii_whitespace());
    let rest = uri
        .get(..5)
        .filter(|scheme| scheme.eq_ignore_ascii_case("file:"))
        .map(|_| &uri[5..])
        .ok_or(UriFault::Invalid)?;
    if rest.contains(['?', '#']) {
        return Err(UriFault::Invalid);
    }
    let (host, path) = match rest.strip_prefix("//") {
        Some(authority) => {
            let i = authority.find('/').ok_or(UriFault::Invalid)?;
            (&authority[..i], &authority[i..])
        }
        None => ("", rest),
    };
    if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
        return Err(UriFault::Remote);
    }
    if !path.starts_with('/') {
        return Err(UriFault::Relative);
    }
    let path = percent_decode(path).ok_or(UriFault::Invalid)?;
    if path.contains('\0') || path == "/" {
        return Err(UriFault::Invalid);
    }
    // A second leading separator makes the rest of the path a server name on Windows and is a
    // different spelling of the same path elsewhere; neither is accepted, in any encoding.
    if path[1..].starts_with(['/', '\\']) {
        return Err(UriFault::Remote);
    }
    let separators: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    if path.split(separators).any(|part| part == "..") {
        return Err(UriFault::Invalid);
    }

    // "/c:/dir" -> "c:/dir"
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 4
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
        && (bytes[3] == b'/' || bytes[3] == b'\\');
    if cfg!(windows) {
        if !drive {
            return Err(UriFault::Relative);
        }
        Ok(path[1..].replace('/', "\\").into())
    } else if drive {
        Err(UriFault::Invalid)
    } else {
        Ok(path.into())
    }
}

fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            // `from_str_radix` would also take a leading sign.
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests;

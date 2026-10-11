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
    /// malformed or unencoded reserved character, a `..` component, a NUL, or (on Windows) a
    /// path that is not in drive-letter form.
    #[error("{element} is not a valid file URI: {uri:?}")]
    InvalidUri {
        /// The element name.
        element: &'static str,
        /// The attribute value.
        uri: String,
    },
    /// A file element names a non-local file: a host other than `localhost`, or a path that
    /// starts with `//` (a share). The 7.2 writability premise cannot be established for it.
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
    /// The root file is neither UTF-8 nor UTF-16 with a byte order mark.
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
/// little- or big-endian byte order mark. `None` for anything else.
fn decode_root_bytes(bytes: &[u8]) -> Option<String> {
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
/// or its content is absent; a `REG_EXPAND_SZ` value is expanded with the view's program-files
/// folders), `vci_service_config::pdu_api_root_file()` elsewhere (always `Some`; the file may not
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
        root_file_from_key(&key, view)
    }
    #[cfg(not(windows))]
    {
        let _ = view;
        Ok(Some(vci_service_config::pdu_api_root_file()))
    }
}

/// Reads the `Root File` value of an opened `D-PDU API` registry key. Only a `REG_EXPAND_SZ`
/// value has its `%NAME%` references expanded; a `REG_SZ` value is taken literally.
#[cfg(windows)]
fn root_file_from_key(
    key: &winreg::RegKey,
    view: RegistryView,
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
    root_file_from_value(&value.bytes, expand, view, &|n| std::env::var(n).ok())
        .map_err(ResolveError::Registry)
}

/// The path held by the raw bytes of a registry string value (UTF-16LE, trailing NULs stripped,
/// trimmed), expanded when `expand` is set. `None` if blank.
#[cfg(any(windows, test))]
fn root_file_from_value(
    bytes: &[u8],
    expand: bool,
    view: RegistryView,
    env: &dyn Fn(&str) -> Option<String>,
) -> io::Result<Option<PathBuf>> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let text =
        utf16(bytes, u16::from_le_bytes).ok_or_else(|| invalid("Root File is not valid UTF-16"))?;
    let text = text.trim_end_matches('\0');
    if text.contains('\0') {
        return Err(invalid("Root File contains a NUL character"));
    }
    let text = if expand {
        expand_env(text, |name| view_variable(view, name, env))
    } else {
        text.to_owned()
    };
    let text = text.trim();
    Ok((!text.is_empty()).then(|| PathBuf::from(text)))
}

/// Expands `%NAME%` references in `text` the way a `REG_EXPAND_SZ` value is meant to be: a name
/// `lookup` knows is replaced by its value; an unknown name, an empty `%%` and an unmatched `%`
/// stay as they are.
#[cfg(any(windows, test))]
fn expand_env(text: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(j) = after.find('%').filter(|&j| j > 0)
            && let Some(value) = lookup(&after[..j])
        {
            out.push_str(&value);
            rest = &after[j + 1..];
            continue;
        }
        out.push('%');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Looks up an environment variable for expansion in registry view `view`: `ProgramFiles` and
/// `CommonProgramFiles` come from the variables of that view (`...(x86)` for the 32-bit view,
/// `...W6432` for the 64-bit one), falling back to the plain name when the view's variable is
/// absent (32-bit Windows). The names are matched case-insensitively.
#[cfg(any(windows, test))]
fn view_variable(
    view: RegistryView,
    name: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    const OVERRIDES: [(&str, &str, &str); 2] = [
        ("ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"),
        (
            "CommonProgramFiles",
            "CommonProgramFiles(x86)",
            "CommonProgramW6432",
        ),
    ];
    for (plain, wow32, wow64) in OVERRIDES {
        if name.eq_ignore_ascii_case(plain) {
            let specific = match view {
                RegistryView::Wow64_32 => wow32,
                RegistryView::Wow64_64 => wow64,
            };
            return env(specific).or_else(|| env(plain));
        }
    }
    env(name)
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
/// drive-letter form (`file:///c:/dir/x.dll`). Percent-encoding is decoded.
///
/// Refused: another scheme; a host other than `localhost` (which includes `file://c:/dir`); a
/// path starting with `//` or `/\` before or after decoding (a share); a raw `?` or `#` (a query
/// or fragment, or an unencoded character; `%3F` and `%23` decode normally); a `..` component or
/// a NUL after decoding; on Windows a path without a drive letter; elsewhere a path that looks
/// like a drive letter.
fn uri_to_path(uri: &str) -> Result<PathBuf, UriFault> {
    let uri = uri.trim();
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
    if path.contains('\0') {
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
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests;

//! Parsing and validation of one Linux registration definition (7.1.1, ADR-266).

use std::{io, path::PathBuf};

use serde::Deserialize;

/// Protocol support keys, named as the Windows registry values (J2534-1 section 9.2).
pub const PROTOCOL_KEYS: &[&str] = &[
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

/// A validated registration definition (one VCI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// VCI name a caller resolves; matched exactly, case-sensitively.
    pub name: String,
    /// Vendor, for display only.
    pub vendor: Option<String>,
    /// Absolute path of the vendor library.
    pub library: PathBuf,
    /// Configuration application, for display only.
    pub config_application: Option<String>,
    /// Names of the supported protocols (from [`PROTOCOL_KEYS`]), in that order.
    pub protocols: Vec<String>,
    /// Width in bytes of `unsigned long` the library was built for (4 or 8), if stated (7.1.2).
    pub long_size: Option<u8>,
    /// Additional search paths for dependent libraries; all absolute.
    pub search_paths: Vec<PathBuf>,
}

/// Why a definition is invalid.
#[derive(Debug, thiserror::Error)]
pub enum DefinitionError {
    /// The file could not be read.
    #[error("cannot read definition: {0}")]
    Io(#[from] io::Error),
    /// Not valid TOML, a required key is missing, a key has the wrong type or is unknown.
    #[error("malformed definition: {0}")]
    Syntax(#[from] toml::de::Error),
    /// `Name` is empty.
    #[error("`Name` is empty")]
    EmptyName,
    /// `FunctionLibrary` is not an absolute path.
    #[error("`FunctionLibrary` is not an absolute path: {}", .0.display())]
    RelativeLibrary(PathBuf),
    /// A protocol key holds something other than 0 or 1.
    #[error("`{key}` must be 0 or 1, got {value}")]
    InvalidProtocolValue {
        /// The protocol key.
        key: &'static str,
        /// The offending value.
        value: i64,
    },
    /// `LongSize` is neither 4 nor 8.
    #[error("`LongSize` must be 4 or 8, got {0}")]
    InvalidLongSize(i64),
    /// A `SearchPaths` entry is not an absolute path.
    #[error("`SearchPaths` entry is not an absolute path: {}", .0.display())]
    RelativeSearchPath(PathBuf),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Vendor")]
    vendor: Option<String>,
    #[serde(rename = "FunctionLibrary")]
    function_library: String,
    #[serde(rename = "ConfigApplication")]
    config_application: Option<String>,
    #[serde(rename = "J1850VPW")]
    j1850vpw: Option<i64>,
    #[serde(rename = "J1850PWM")]
    j1850pwm: Option<i64>,
    #[serde(rename = "ISO9141")]
    iso9141: Option<i64>,
    #[serde(rename = "ISO14230")]
    iso14230: Option<i64>,
    #[serde(rename = "CAN")]
    can: Option<i64>,
    #[serde(rename = "ISO15765")]
    iso15765: Option<i64>,
    #[serde(rename = "SCI_A_ENGINE")]
    sci_a_engine: Option<i64>,
    #[serde(rename = "SCI_A_TRANS")]
    sci_a_trans: Option<i64>,
    #[serde(rename = "SCI_B_ENGINE")]
    sci_b_engine: Option<i64>,
    #[serde(rename = "SCI_B_TRANS")]
    sci_b_trans: Option<i64>,
    #[serde(rename = "LongSize")]
    long_size: Option<i64>,
    #[serde(rename = "SearchPaths", default)]
    search_paths: Vec<String>,
}

impl Definition {
    /// Parses and validates the text of one definition file.
    pub fn parse(text: &str) -> Result<Definition, DefinitionError> {
        let raw: Raw = toml::from_str(text)?;
        if raw.name.is_empty() {
            return Err(DefinitionError::EmptyName);
        }
        let library = PathBuf::from(&raw.function_library);
        if !library.is_absolute() {
            return Err(DefinitionError::RelativeLibrary(library));
        }
        let flags = [
            raw.j1850vpw,
            raw.j1850pwm,
            raw.iso9141,
            raw.iso14230,
            raw.can,
            raw.iso15765,
            raw.sci_a_engine,
            raw.sci_a_trans,
            raw.sci_b_engine,
            raw.sci_b_trans,
        ];
        let mut protocols = Vec::new();
        for (&key, flag) in PROTOCOL_KEYS.iter().zip(flags) {
            match flag {
                None | Some(0) => {}
                Some(1) => protocols.push(key.to_owned()),
                Some(value) => return Err(DefinitionError::InvalidProtocolValue { key, value }),
            }
        }
        let long_size = match raw.long_size {
            None => None,
            Some(4) => Some(4),
            Some(8) => Some(8),
            Some(other) => return Err(DefinitionError::InvalidLongSize(other)),
        };
        let mut search_paths = Vec::with_capacity(raw.search_paths.len());
        for entry in raw.search_paths {
            let path = PathBuf::from(entry);
            if !path.is_absolute() {
                return Err(DefinitionError::RelativeSearchPath(path));
            }
            search_paths.push(path);
        }
        Ok(Definition {
            name: raw.name,
            vendor: raw.vendor,
            library,
            config_application: raw.config_application,
            protocols,
            long_size,
            search_paths,
        })
    }
}

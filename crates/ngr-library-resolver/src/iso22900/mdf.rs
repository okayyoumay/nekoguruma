//! The error codes of a module description file (MDF; ISO 22900-2:2022 Annex F, F.4): the
//! vendor's names for the extra error information its error events carry (D.4.2, 10.1.4.11.3).
//!
//! Only the `ERROR_CODE` elements under the document element `MVCI_MODULE_DESCRIPTION` are read;
//! each has an `ID` (a decimal unsigned 32-bit number), a `SHORT_NAME` and a `DESCRIPTION`. The
//! description is the vendor's text and is not kept. An element without a usable `ID` or
//! `SHORT_NAME` is skipped, and an `ID` that two elements give different names gets no name, so
//! a reader never shows the wrong one.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use super::{decode_root_bytes, element_text, is_named};

/// Largest module description file accepted, in bytes. An MDF lists every ComParam and resource
/// of the vendor's modules, so it is allowed more than a root file.
pub const MAX_MDF_SIZE: u64 = 16 * 1024 * 1024;

/// The document element of a module description file.
const MDF_ELEMENT: &str = "MVCI_MODULE_DESCRIPTION";

/// The error codes an MDF names: each code's `SHORT_NAME`, by its `ID`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorCodes {
    names: BTreeMap<u32, String>,
}

impl ErrorCodes {
    /// The `SHORT_NAME` the MDF gives `code`, if it gives exactly one.
    pub fn name(&self, code: u32) -> Option<&str> {
        self.names.get(&code).map(String::as_str)
    }

    /// The number of codes with a name.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether no code has a name.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// Why an MDF's error codes cannot be read.
#[derive(Debug, thiserror::Error)]
pub enum MdfError {
    /// The file cannot be read.
    #[error("cannot read module description file {}: {source}", path.display())]
    Io {
        /// The MDF.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The file is larger than [`MAX_MDF_SIZE`].
    #[error(
        "module description file {} is larger than {MAX_MDF_SIZE} bytes",
        path.display()
    )]
    TooLarge {
        /// The MDF.
        path: PathBuf,
    },
    /// The file is neither UTF-8 nor UTF-16 with a byte order mark.
    #[error(
        "module description file {} is not UTF-8 or byte-order-marked UTF-16",
        path.display()
    )]
    Encoding {
        /// The MDF.
        path: PathBuf,
    },
    /// The file is not well-formed XML.
    #[error("invalid XML in module description file {}: {source}", path.display())]
    Xml {
        /// The MDF; `<memory>` for [`parse_error_codes`].
        path: PathBuf,
        /// The parser error.
        source: roxmltree::Error,
    },
    /// The document element is not `MVCI_MODULE_DESCRIPTION`.
    #[error(
        "{} is not a module description file (document element {element:?})",
        path.display()
    )]
    NotAnMdf {
        /// The MDF; `<memory>` for [`parse_error_codes`].
        path: PathBuf,
        /// The local name of the document element found.
        element: String,
    },
}

/// Parses the error codes of an MDF's contents. The path in an error is `<memory>`.
pub fn parse_error_codes(xml: &str) -> Result<ErrorCodes, MdfError> {
    parse(xml, Path::new("<memory>"))
}

/// Reads the error codes of the MDF at `path`, refusing more than [`MAX_MDF_SIZE`] bytes. The
/// file must be UTF-8 or byte-order-marked UTF-16, as a root file must.
pub fn read_error_codes(path: &Path) -> Result<ErrorCodes, MdfError> {
    let io_err = |source| MdfError::Io {
        path: path.to_owned(),
        source,
    };
    let too_large = || MdfError::TooLarge {
        path: path.to_owned(),
    };
    let file = fs::File::open(path).map_err(io_err)?;
    if file.metadata().map_err(io_err)?.len() > MAX_MDF_SIZE {
        return Err(too_large());
    }
    // The file may grow after the metadata check, so the read is bounded as well.
    let mut bytes = Vec::new();
    file.take(MAX_MDF_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(io_err)?;
    if bytes.len() as u64 > MAX_MDF_SIZE {
        return Err(too_large());
    }
    let text = decode_root_bytes(&bytes).ok_or_else(|| MdfError::Encoding {
        path: path.to_owned(),
    })?;
    parse(&text, path)
}

fn parse(xml: &str, path: &Path) -> Result<ErrorCodes, MdfError> {
    let doc = roxmltree::Document::parse(xml).map_err(|source| MdfError::Xml {
        path: path.to_owned(),
        source,
    })?;
    let document = doc.root_element();
    if !is_named(&document, MDF_ELEMENT) {
        return Err(MdfError::NotAnMdf {
            path: path.to_owned(),
            element: document.tag_name().name().to_owned(),
        });
    }
    // `None` marks an ID two elements name differently.
    let mut names: BTreeMap<u32, Option<String>> = BTreeMap::new();
    for code in document.children().filter(|n| is_named(n, "ERROR_CODE")) {
        let only = |name: &str| {
            let mut found = code.children().filter(|n| is_named(n, name));
            match (found.next(), found.next()) {
                (Some(node), None) if !node.children().any(|c| c.is_element()) => {
                    Some(element_text(node).trim().to_owned()).filter(|text| !text.is_empty())
                }
                _ => None,
            }
        };
        let (Some(id), Some(short_name)) = (only("ID"), only("SHORT_NAME")) else {
            continue;
        };
        // xsd:unsignedInt: decimal digits, an optional leading `+`.
        let digits = id.strip_prefix('+').unwrap_or(&id);
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(id) = digits.parse::<u32>() else {
            continue;
        };
        match names.entry(id) {
            Entry::Vacant(slot) => {
                slot.insert(Some(short_name));
            }
            Entry::Occupied(mut slot) => {
                if slot.get().as_deref() != Some(short_name.as_str()) {
                    slot.insert(None);
                }
            }
        }
    }
    Ok(ErrorCodes {
        names: names
            .into_iter()
            .filter_map(|(id, name)| Some((id, name?)))
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mdf(codes: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <MVCI_MODULE_DESCRIPTION MVCI_PART2_STANDARD_VERSION=\"2.2.2\" FILE_VERSION=\"1\">\
             <SHORT_NAME>MDF</SHORT_NAME>{codes}</MVCI_MODULE_DESCRIPTION>"
        )
    }

    fn code(id: &str, name: &str) -> String {
        format!(
            "<ERROR_CODE><ID>{id}</ID><SHORT_NAME>{name}</SHORT_NAME>\
             <DESCRIPTION>text</DESCRIPTION></ERROR_CODE>"
        )
    }

    #[test]
    fn each_error_code_is_named_by_its_id() {
        let codes = parse_error_codes(&mdf(&format!(
            "{}{}<COMPARAM EID=\"C1\"><ID>257</ID><SHORT_NAME>CP_X</SHORT_NAME></COMPARAM>",
            code("257", " XTRA_RX_TIMEOUT "),
            code("+4294967295", "XTRA_MAX")
        )))
        .unwrap();
        assert_eq!(codes.name(257), Some("XTRA_RX_TIMEOUT"));
        assert_eq!(codes.name(u32::MAX), Some("XTRA_MAX"));
        assert_eq!(codes.name(258), None);
        // Only error codes are read: the ComParam's ID 257 names nothing.
        assert_eq!(codes.len(), 2);
    }

    #[test]
    fn an_unusable_or_ambiguous_code_gets_no_name() {
        let codes = parse_error_codes(&mdf(&[
            code("0x10", "HEX"),
            code("-1", "NEGATIVE"),
            code("4294967296", "TOO_LARGE"),
            code("", "EMPTY_ID"),
            code("11", ""),
            "<ERROR_CODE><ID>12</ID><ID>13</ID><SHORT_NAME>TWO_IDS</SHORT_NAME></ERROR_CODE>"
                .to_owned(),
            "<ERROR_CODE><ID>14</ID><SHORT_NAME>A<B/></SHORT_NAME></ERROR_CODE>".to_owned(),
            code("20", "SAME"),
            code("20", "SAME"),
            code("21", "FIRST"),
            code("21", "SECOND"),
        ]
        .concat()))
        .unwrap();
        // A repeated ID with the same name keeps it; with another name it has none.
        assert_eq!(codes.name(20), Some("SAME"));
        assert_eq!(codes.name(21), None);
        assert_eq!(codes.len(), 1);
    }

    #[test]
    fn a_file_that_is_no_mdf_is_refused() {
        assert!(matches!(
            parse_error_codes("<MVCI_PDU_API_ROOT/>"),
            Err(MdfError::NotAnMdf { element, .. }) if element == "MVCI_PDU_API_ROOT"
        ));
        assert!(matches!(
            parse_error_codes("<MVCI_MODULE_DESCRIPTION>"),
            Err(MdfError::Xml { .. })
        ));
        assert!(parse_error_codes(&mdf("")).unwrap().is_empty());
    }

    #[test]
    fn a_file_is_read_in_utf8_or_marked_utf16() {
        let dir = tempfile::tempdir().unwrap();
        let utf8 = dir.path().join("utf8.xml");
        fs::write(&utf8, mdf(&code("1", "ONE"))).unwrap();
        assert_eq!(read_error_codes(&utf8).unwrap().name(1), Some("ONE"));
        let utf16 = dir.path().join("utf16.xml");
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(
            mdf(&code("2", "TWO"))
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        fs::write(&utf16, bytes).unwrap();
        assert_eq!(read_error_codes(&utf16).unwrap().name(2), Some("TWO"));
        let latin1 = dir.path().join("latin1.xml");
        fs::write(
            &latin1,
            b"<MVCI_MODULE_DESCRIPTION>\xE9</MVCI_MODULE_DESCRIPTION>",
        )
        .unwrap();
        assert!(matches!(
            read_error_codes(&latin1),
            Err(MdfError::Encoding { .. })
        ));
        assert!(matches!(
            read_error_codes(&dir.path().join("missing.xml")),
            Err(MdfError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound
        ));
    }
}

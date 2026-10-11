//! D-PDU API discovery (7.1, ISO 22900-2 section 8.7 and Annex F.1).
//!
//! Chain: root description file (`pdu_api_root.xml`) -> one `MVCI_PDU_API` entry per
//! implementation -> API library, module description file (MDF) and cable description file (CDF).
//! The root file is located via `HKLM\SOFTWARE\D-PDU API` value `Root File` on Windows and by
//! `vci_service_config::pdu_api_root_file()` elsewhere (`/etc/pdu_api_root.xml` by default).

use std::path::{Path, PathBuf};

use crate::{DiscoveryError, RegistryView};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PduApiLibrary {
    pub short_name: String,
    pub description: Option<String>,
    pub supplier_name: Option<String>,
    pub library_file: PathBuf,
    pub module_description_file: Option<PathBuf>,
    pub cable_description_file: Option<PathBuf>,
}

/// Location of the root description file. `None` if no D-PDU API is installed.
/// `view` is ignored outside Windows.
pub fn root_file_path(view: RegistryView) -> Result<Option<PathBuf>, DiscoveryError> {
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
        let key = match RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags(r"SOFTWARE\D-PDU API", KEY_READ | view.flag())
        {
            Ok(key) => key,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match key.get_value::<String, _>("Root File") {
            Ok(path) => Ok(Some(PathBuf::from(path.trim()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = view;
        let path = vci_service_config::pdu_api_root_file();
        Ok(path.exists().then_some(path))
    }
}

/// Reads and parses a root description file.
pub fn read_root_file(path: &Path) -> Result<Vec<PduApiLibrary>, DiscoveryError> {
    let xml = std::fs::read_to_string(path)?;
    parse_root_file(&xml).map_err(|e| match e {
        DiscoveryError::Xml { source, .. } => DiscoveryError::Xml {
            path: path.display().to_string(),
            source,
        },
        other => other,
    })
}

/// Parses the contents of a root description file, sorted by short name.
/// Entries without `SHORT_NAME` or `LIBRARY_FILE` are skipped.
pub fn parse_root_file(xml: &str) -> Result<Vec<PduApiLibrary>, DiscoveryError> {
    let doc = roxmltree::Document::parse(xml).map_err(|source| DiscoveryError::Xml {
        path: "<memory>".into(),
        source,
    })?;
    let mut libraries = Vec::new();
    for api in doc.descendants().filter(|n| n.has_tag_name("MVCI_PDU_API")) {
        let child = |name: &str| api.children().find(|n| n.has_tag_name(name));
        let text = |name: &str| {
            child(name)
                .and_then(|n| n.text())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let uri = |name: &str| {
            child(name)
                .and_then(|n| n.attribute("URI"))
                .map(uri_to_path)
                .transpose()
        };

        let (Some(short_name), Some(library_file)) = (text("SHORT_NAME"), uri("LIBRARY_FILE")?)
        else {
            continue;
        };
        libraries.push(PduApiLibrary {
            short_name,
            description: text("DESCRIPTION"),
            supplier_name: text("SUPPLIER_NAME"),
            library_file,
            module_description_file: uri("MODULE_DESCRIPTION_FILE")?,
            cable_description_file: uri("CABLE_DESCRIPTION_FILE")?,
        });
    }
    libraries.sort_by(|a, b| a.short_name.cmp(&b.short_name));
    Ok(libraries)
}

/// Converts a `file:` URI to a path. Accepts the forms seen in root files:
/// `file:/c:/vendor/pdu.dll`, `file:///c:/vendor/pdu.dll`, `file:///opt/vendor/libpdu.so`
/// and `file://localhost/...`. Percent-encoding is decoded. On Windows a remote host becomes
/// a UNC path (`\\host\share\...`); elsewhere a remote host is rejected.
fn uri_to_path(uri: &str) -> Result<PathBuf, DiscoveryError> {
    let invalid = || DiscoveryError::InvalidUri(uri.to_owned());
    let uri = uri.trim();
    let rest = uri
        .get(..5)
        .filter(|scheme| scheme.eq_ignore_ascii_case("file:"))
        .map(|_| &uri[5..])
        .ok_or_else(invalid)?;
    let (host, path) = match rest.strip_prefix("//") {
        Some(authority) => match authority.find('/') {
            Some(i) => (&authority[..i], &authority[i..]),
            None => return Err(invalid()),
        },
        None => ("", rest),
    };
    if !path.starts_with('/') {
        return Err(invalid());
    }
    let path = percent_decode(path).ok_or_else(invalid)?;
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
        Ok(if host.is_empty() {
            path.into()
        } else {
            format!("\\\\{host}{path}").into()
        })
    } else if host.is_empty() && !drive {
        Ok(path.into())
    } else {
        Err(invalid())
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
mod tests {
    use super::*;

    #[cfg(unix)]
    const ROOT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MVCI_PDU_API_ROOT MVCI_PART2_STANDARD_VERSION="2.2.0">
  <MVCI_PDU_API>
    <SHORT_NAME>VENDOR_B</SHORT_NAME>
    <LIBRARY_FILE URI="file:///opt/vendor-b/libpdu.so"/>
    <MODULE_DESCRIPTION_FILE URI="file:///opt/vendor-b/mdf.xml"/>
    <CABLE_DESCRIPTION_FILE URI="file:///opt/vendor-b/cdf.xml"/>
  </MVCI_PDU_API>
  <MVCI_PDU_API>
    <SHORT_NAME> VENDOR_A </SHORT_NAME>
    <DESCRIPTION>Example VCI</DESCRIPTION>
    <SUPPLIER_NAME>Vendor A</SUPPLIER_NAME>
    <LIBRARY_FILE URI="file:///opt/vendor-a/libpdu.so"/>
  </MVCI_PDU_API>
  <MVCI_PDU_API>
    <SHORT_NAME>NO_LIBRARY</SHORT_NAME>
  </MVCI_PDU_API>
</MVCI_PDU_API_ROOT>"#;

    #[cfg(unix)]
    #[test]
    fn parses_entries_sorted_and_skips_incomplete() {
        let libs = parse_root_file(ROOT).unwrap();
        assert_eq!(libs.len(), 2);
        assert_eq!(libs[0].short_name, "VENDOR_A");
        assert_eq!(libs[0].supplier_name.as_deref(), Some("Vendor A"));
        assert_eq!(
            libs[0].library_file,
            PathBuf::from("/opt/vendor-a/libpdu.so")
        );
        assert_eq!(libs[0].module_description_file, None);
        assert_eq!(
            libs[1].cable_description_file,
            Some(PathBuf::from("/opt/vendor-b/cdf.xml"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_uri_forms() {
        let ok = |u| uri_to_path(u).unwrap();
        assert_eq!(
            ok("file:///opt/a%20b/lib.so"),
            PathBuf::from("/opt/a b/lib.so")
        );
        assert_eq!(ok("file:/opt/lib.so"), PathBuf::from("/opt/lib.so"));
        assert_eq!(
            ok("FILE://localhost/opt/lib.so"),
            PathBuf::from("/opt/lib.so")
        );
        assert!(uri_to_path("file://server/share/lib.so").is_err());
        assert!(uri_to_path("file:/c:/tmp1/pdu.dll").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn file_uri_forms() {
        let ok = |u| uri_to_path(u).unwrap();
        assert_eq!(
            ok("file:/c:/tmp1/pdu.dll"),
            PathBuf::from(r"c:\tmp1\pdu.dll")
        );
        assert_eq!(
            ok("file:///C:/Program%20Files/pdu.dll"),
            PathBuf::from(r"C:\Program Files\pdu.dll")
        );
        assert_eq!(
            ok("file://server/share/pdu.dll"),
            PathBuf::from(r"\\server\share\pdu.dll")
        );
    }

    #[test]
    fn rejects_non_file_uri() {
        let xml = r#"<MVCI_PDU_API_ROOT><MVCI_PDU_API><SHORT_NAME>X</SHORT_NAME>
            <LIBRARY_FILE URI="http://example.com/pdu.dll"/></MVCI_PDU_API></MVCI_PDU_API_ROOT>"#;
        assert!(matches!(
            parse_root_file(xml),
            Err(DiscoveryError::InvalidUri(_))
        ));
    }

    #[test]
    fn rejects_malformed_xml() {
        assert!(matches!(
            parse_root_file("<MVCI_PDU_API_ROOT>"),
            Err(DiscoveryError::Xml { .. })
        ));
    }
}

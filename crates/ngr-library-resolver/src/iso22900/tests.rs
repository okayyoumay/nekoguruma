use std::fs;

use super::*;

/// A `file:` URI and the path it must convert to, valid on the running platform.
fn target(rel: &str) -> (String, PathBuf) {
    if cfg!(windows) {
        (
            format!("file:///C:/opt/{rel}"),
            PathBuf::from(format!(r"C:\opt\{}", rel.replace('/', "\\"))),
        )
    } else {
        (
            format!("file:///opt/{rel}"),
            PathBuf::from(format!("/opt/{rel}")),
        )
    }
}

fn uri(rel: &str) -> String {
    target(rel).0
}

fn path(rel: &str) -> PathBuf {
    target(rel).1
}

fn root(entries: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <MVCI_PDU_API_ROOT>{entries}</MVCI_PDU_API_ROOT>"
    )
}

/// An entry with a library only.
fn entry(name: &str, lib: &str) -> String {
    format!(
        "<MVCI_PDU_API><SHORT_NAME>{name}</SHORT_NAME>\
         <LIBRARY_FILE URI=\"{}\"/></MVCI_PDU_API>",
        uri(lib)
    )
}

/// Writes `xml` as a root file in a fresh directory.
fn write_root(xml: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pdu_api_root.xml");
    fs::write(&file, xml).unwrap();
    (dir, file)
}

#[test]
fn resolves_a_name_with_all_files() {
    let xml = root(&format!(
        "{}<MVCI_PDU_API>\n<SHORT_NAME> VENDOR_A </SHORT_NAME>\n\
         <DESCRIPTION>Example VCI</DESCRIPTION><SUPPLIER_NAME>Vendor A</SUPPLIER_NAME>\n\
         <LIBRARY_FILE URI=\"{}\"/><MODULE_DESCRIPTION_FILE URI=\"{}\"/>\n\
         <CABLE_DESCRIPTION_FILE URI=\"{}\"/></MVCI_PDU_API>",
        entry("VENDOR_B", "b/libpdu.so"),
        uri("a/libpdu.so"),
        uri("a/mdf.xml"),
        uri("a/cdf.xml"),
    ));
    let (_dir, file) = write_root(&xml);
    let resolved = resolve_in_root_file(&file, "VENDOR_A").unwrap();
    let i = &resolved.implementation;
    assert_eq!(i.short_name, "VENDOR_A");
    assert_eq!(i.description.as_deref(), Some("Example VCI"));
    assert_eq!(i.supplier_name.as_deref(), Some("Vendor A"));
    assert_eq!(i.library_file, path("a/libpdu.so"));
    assert_eq!(i.module_description_file, Some(path("a/mdf.xml")));
    assert_eq!(i.cable_description_file, Some(path("a/cdf.xml")));
    assert_eq!(resolved.root_file, file);
    assert_eq!(
        resolved.naming_files(),
        [file, path("a/mdf.xml"), path("a/cdf.xml")]
    );
}

#[test]
fn naming_files_without_mdf_and_cdf_is_the_root_file() {
    let (_dir, file) = write_root(&root(&entry("X", "x/lib.so")));
    let resolved = resolve_in_root_file(&file, "X").unwrap();
    assert_eq!(resolved.implementation.module_description_file, None);
    assert_eq!(resolved.naming_files(), [file]);
}

#[test]
fn matching_is_exact_and_case_sensitive() {
    let (_dir, file) = write_root(&root(&entry("Vendor", "x/lib.so")));
    for name in ["vendor", "Vendor ", "Vend"] {
        assert!(
            matches!(
                resolve_in_root_file(&file, name),
                Err(ResolveError::NotFound { .. })
            ),
            "{name:?}"
        );
    }
    assert!(resolve_in_root_file(&file, "Vendor").is_ok());
}

#[test]
fn not_found_counts_skipped_entries() {
    let xml = root(&format!(
        "{}<MVCI_PDU_API><SHORT_NAME>NO_LIB</SHORT_NAME></MVCI_PDU_API>\
         <MVCI_PDU_API><LIBRARY_FILE URI=\"{}\"/></MVCI_PDU_API>",
        entry("OTHER", "o/lib.so"),
        uri("n/lib.so"),
    ));
    let (_dir, file) = write_root(&xml);
    let err = resolve_in_root_file(&file, "WANTED").unwrap_err();
    match &err {
        ResolveError::NotFound {
            name,
            skipped_invalid,
        } => {
            assert_eq!(name, "WANTED");
            assert_eq!(*skipped_invalid, 2);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(err.to_string().contains("2 unusable"));
}

#[test]
fn an_unusable_entry_of_another_name_does_not_block() {
    let xml = root(&format!(
        "<MVCI_PDU_API><SHORT_NAME>BAD</SHORT_NAME>\
         <LIBRARY_FILE URI=\"http://example.com/x.dll\"/></MVCI_PDU_API>{}",
        entry("GOOD", "g/lib.so"),
    ));
    let (_dir, file) = write_root(&xml);
    assert!(resolve_in_root_file(&file, "GOOD").is_ok());
}

#[test]
fn duplicate_short_names_are_ambiguous() {
    let xml = root(&format!(
        "{}{}",
        entry("DUP", "a/lib.so"),
        entry(" DUP ", "b/lib.so")
    ));
    let (_dir, file) = write_root(&xml);
    match resolve_in_root_file(&file, "DUP").unwrap_err() {
        ResolveError::Ambiguous {
            name,
            root_file,
            count,
        } => {
            assert_eq!(name, "DUP");
            assert_eq!(root_file, file);
            assert_eq!(count, 2);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn an_unusable_duplicate_still_makes_the_name_ambiguous() {
    let xml = root(&format!(
        "{}<MVCI_PDU_API><SHORT_NAME>DUP</SHORT_NAME></MVCI_PDU_API>",
        entry("DUP", "a/lib.so")
    ));
    let (_dir, file) = write_root(&xml);
    assert!(matches!(
        resolve_in_root_file(&file, "DUP"),
        Err(ResolveError::Ambiguous { count: 2, .. })
    ));
}

fn invalid_source(xml: &str, name: &str) -> EntryError {
    let (_dir, file) = write_root(xml);
    match resolve_in_root_file(&file, name).unwrap_err() {
        ResolveError::InvalidEntry { source, .. } => source,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_missing_library_is_refused() {
    let xml = root("<MVCI_PDU_API><SHORT_NAME>X</SHORT_NAME></MVCI_PDU_API>");
    assert_eq!(invalid_source(&xml, "X"), EntryError::MissingLibrary);
    // The element without a URI attribute counts as missing as well.
    let xml = root("<MVCI_PDU_API><SHORT_NAME>X</SHORT_NAME><LIBRARY_FILE/></MVCI_PDU_API>");
    assert_eq!(invalid_source(&xml, "X"), EntryError::MissingLibrary);
}

#[test]
fn an_invalid_uri_is_refused() {
    for bad in [
        "http://example.com/x.dll",
        "/opt/lib.so",
        "file:relative/lib.so",
        "file:///opt/a%zz/lib.so",
        "file:///opt/a%00/lib.so",
    ] {
        let xml = root(&format!(
            "<MVCI_PDU_API><SHORT_NAME>X</SHORT_NAME>\
             <LIBRARY_FILE URI=\"{bad}\"/></MVCI_PDU_API>"
        ));
        assert!(
            matches!(
                invalid_source(&xml, "X"),
                EntryError::InvalidUri {
                    element: "LIBRARY_FILE",
                    ..
                }
            ),
            "{bad}"
        );
    }
}

#[test]
fn an_invalid_mdf_uri_is_refused() {
    let xml = root(&format!(
        "<MVCI_PDU_API><SHORT_NAME>X</SHORT_NAME><LIBRARY_FILE URI=\"{}\"/>\
         <MODULE_DESCRIPTION_FILE URI=\"ftp://h/mdf.xml\"/></MVCI_PDU_API>",
        uri("x/lib.so")
    ));
    assert!(matches!(
        invalid_source(&xml, "X"),
        EntryError::InvalidUri {
            element: "MODULE_DESCRIPTION_FILE",
            ..
        }
    ));
}

/// On Windows a URI without a drive letter or host converts to a path that is not absolute.
#[cfg(windows)]
#[test]
fn a_relative_library_is_refused() {
    let xml = root(
        "<MVCI_PDU_API><SHORT_NAME>X</SHORT_NAME>\
         <LIBRARY_FILE URI=\"file:/dir/x.dll\"/></MVCI_PDU_API>",
    );
    assert!(matches!(
        invalid_source(&xml, "X"),
        EntryError::RelativeLibrary { .. }
    ));
}

#[test]
fn file_uri_forms() {
    let (u, p) = target("a b/lib.so");
    assert_eq!(uri_to_path(&u.replace(' ', "%20")), Some(p));
    assert!(uri_to_path("FILE://localhost/opt/lib.so").is_some());
    #[cfg(unix)]
    {
        assert_eq!(
            uri_to_path("file:/opt/lib.so"),
            Some(PathBuf::from("/opt/lib.so"))
        );
        assert_eq!(uri_to_path("file://server/share/lib.so"), None);
        assert_eq!(uri_to_path("file:/c:/tmp1/pdu.dll"), None);
    }
    #[cfg(windows)]
    {
        assert_eq!(
            uri_to_path("file:/c:/tmp1/pdu.dll"),
            Some(PathBuf::from(r"c:\tmp1\pdu.dll"))
        );
        assert_eq!(
            uri_to_path("file://server/share/pdu.dll"),
            Some(PathBuf::from(r"\\server\share\pdu.dll"))
        );
    }
}

#[test]
fn a_root_file_over_the_size_limit_is_refused() {
    let padding = " ".repeat(MAX_ROOT_FILE_SIZE as usize);
    let (_dir, file) = write_root(&root(&format!("{}{padding}", entry("X", "x/lib.so"))));
    assert!(matches!(
        resolve_in_root_file(&file, "X"),
        Err(ResolveError::TooLarge { .. })
    ));
}

#[test]
fn malformed_xml_names_the_file() {
    let (_dir, file) = write_root("<MVCI_PDU_API_ROOT><MVCI_PDU_API>");
    let err = resolve_in_root_file(&file, "X").unwrap_err();
    assert!(matches!(&err, ResolveError::Xml { path, .. } if *path == file));
    assert!(err.to_string().contains("pdu_api_root.xml"));
    assert!(matches!(
        parse_root_file("<a>"),
        Err(ResolveError::Xml { .. })
    ));
}

#[test]
fn a_missing_root_file_is_not_a_missing_name() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("absent.xml");
    match resolve_in_root_file(&file, "X").unwrap_err() {
        ResolveError::NoRootFile { path } => assert_eq!(path, Some(file)),
        other => panic!("unexpected {other:?}"),
    }
}

#[cfg(not(windows))]
#[test]
fn the_root_file_path_is_the_configured_one() {
    assert_eq!(
        root_file_path().unwrap(),
        Some(vci_service_config::pdu_api_root_file())
    );
}

#[cfg(windows)]
mod registry {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};

    use super::*;

    /// A temporary key under HKCU, deleted on drop.
    struct TempKey(RegKey, String);

    impl TempKey {
        fn new(tag: &str) -> Self {
            let name = format!(
                "Software\\ngr-library-resolver-test-{}-{tag}",
                std::process::id()
            );
            let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
                .create_subkey(&name)
                .unwrap();
            Self(key, name)
        }
    }

    impl Drop for TempKey {
        fn drop(&mut self) {
            let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(&self.1);
        }
    }

    #[test]
    fn root_file_is_read_and_trimmed() {
        let temp = TempKey::new("value");
        temp.0
            .set_value("Root File", &r"  C:\ProgramData\pdu_api_root.xml  ")
            .unwrap();
        assert_eq!(
            root_file_from_key(&temp.0).unwrap(),
            Some(PathBuf::from(r"C:\ProgramData\pdu_api_root.xml"))
        );
    }

    #[test]
    fn a_missing_or_blank_value_is_no_root_file() {
        let temp = TempKey::new("missing");
        assert_eq!(root_file_from_key(&temp.0).unwrap(), None);
        temp.0.set_value("Root File", &"   ").unwrap();
        assert_eq!(root_file_from_key(&temp.0).unwrap(), None);
    }
}

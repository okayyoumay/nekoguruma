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

/// Parses `xml` (a whole document) and returns the one unusable entry.
fn only_invalid(xml: &str) -> InvalidEntry {
    let mut parsed = parse_root_file(xml).unwrap();
    assert!(parsed.implementations.is_empty(), "{parsed:?}");
    assert_eq!(parsed.invalid.len(), 1, "{parsed:?}");
    parsed.invalid.remove(0)
}

/// An entry whose `LIBRARY_FILE` is `lib_uri`, plus `extra` child elements.
fn entry_with(lib_uri: &str, extra: &str) -> String {
    format!(
        "<MVCI_PDU_API><SHORT_NAME>X</SHORT_NAME>\
         <LIBRARY_FILE URI=\"{lib_uri}\"/>{extra}</MVCI_PDU_API>"
    )
}

#[test]
fn an_invalid_uri_is_refused() {
    for bad in [
        "http://example.com/x.dll",
        "/opt/lib.so",
        "file:///opt/a%zz/lib.so",
        "file:///opt/a%00/lib.so",
        "file://localhost",
    ] {
        let error = only_invalid(&root(&entry_with(bad, ""))).error;
        assert!(
            matches!(
                error,
                EntryError::InvalidUri {
                    element: "LIBRARY_FILE",
                    ..
                }
            ),
            "{bad}: {error:?}"
        );
    }
}

#[test]
fn an_invalid_mdf_uri_is_refused() {
    let xml = root(&entry_with(
        &uri("x/lib.so"),
        "<MODULE_DESCRIPTION_FILE URI=\"ftp://h/mdf.xml\"/>",
    ));
    assert!(matches!(
        only_invalid(&xml).error,
        EntryError::InvalidUri {
            element: "MODULE_DESCRIPTION_FILE",
            ..
        }
    ));
}

#[test]
fn remote_hosts_and_shares_are_refused_on_every_platform() {
    for bad in [
        "file://c:/dir/x.dll",
        "file://server/share/x.dll",
        "file:////server/share/x.dll",
        "file:///%5C%5Cserver/share/x.dll",
        "file:///%2F%2Fserver/share/x.dll",
        "file://localhost.example/opt/x.dll",
    ] {
        assert_eq!(uri_to_path(bad), Err(UriFault::Remote), "{bad}");
        let error = only_invalid(&root(&entry_with(bad, ""))).error;
        assert!(
            matches!(
                error,
                EntryError::RemoteHost {
                    element: "LIBRARY_FILE",
                    ..
                }
            ),
            "{bad}: {error:?}"
        );
    }
}

#[test]
fn a_remote_mdf_or_cdf_is_refused() {
    for (element, extra) in [
        ("MODULE_DESCRIPTION_FILE", "MODULE_DESCRIPTION_FILE"),
        ("CABLE_DESCRIPTION_FILE", "CABLE_DESCRIPTION_FILE"),
    ] {
        let xml = root(&entry_with(
            &uri("x/lib.so"),
            &format!("<{extra} URI=\"file://server/share/f.xml\"/>"),
        ));
        assert!(
            matches!(
                only_invalid(&xml).error,
                EntryError::RemoteHost { element: e, .. } if e == element
            ),
            "{element}"
        );
    }
}

#[test]
fn localhost_is_accepted() {
    let local = uri("a/lib.so").replacen("file:///", "file://localhost/", 1);
    assert_eq!(uri_to_path(&local), Ok(path("a/lib.so")));
    assert_eq!(
        uri_to_path(&local.replace("localhost", "LocalHost")),
        Ok(path("a/lib.so"))
    );
    let xml = root(&entry_with(&local, ""));
    assert_eq!(
        parse_root_file(&xml).unwrap().implementations[0].library_file,
        path("a/lib.so")
    );
}

#[test]
fn a_raw_query_or_fragment_is_refused_but_encoded_ones_decode() {
    for bad in [uri("a/lib.so?x=1"), uri("a/lib.so#frag"), uri("a?/lib.so")] {
        assert_eq!(uri_to_path(&bad), Err(UriFault::Invalid), "{bad}");
    }
    assert_eq!(
        uri_to_path(&uri("a%3Fb%23c/lib.so")),
        Ok(path("a?b#c/lib.so"))
    );
}

#[test]
fn parent_components_and_nul_are_refused_in_any_encoding() {
    for bad in [
        uri("../lib.so"),
        uri("a/../b/lib.so"),
        uri("a/%2e%2E/lib.so"),
        uri("a/lib.so/.."),
        uri("a%00/lib.so"),
    ] {
        assert_eq!(uri_to_path(&bad), Err(UriFault::Invalid), "{bad}");
    }
    // A dotted name is not a parent component.
    assert_eq!(uri_to_path(&uri("a..b/lib.so")), Ok(path("a..b/lib.so")));
}

#[test]
fn a_relative_mdf_or_cdf_is_refused() {
    for element in ["MODULE_DESCRIPTION_FILE", "CABLE_DESCRIPTION_FILE"] {
        let xml = root(&entry_with(
            &uri("x/lib.so"),
            &format!("<{element} URI=\"file:relative/f.xml\"/>"),
        ));
        assert!(
            matches!(
                only_invalid(&xml).error,
                EntryError::RelativePath { element: e, .. } if e == element
            ),
            "{element}"
        );
    }
    assert_eq!(uri_to_path("file:relative/f.xml"), Err(UriFault::Relative));
}

#[cfg(windows)]
#[test]
fn windows_accepts_only_the_drive_letter_form() {
    assert_eq!(
        uri_to_path("file:/c:/tmp1/pdu.dll"),
        Ok(PathBuf::from(r"c:\tmp1\pdu.dll"))
    );
    assert_eq!(
        uri_to_path("file://localhost/c:/tmp1/pdu.dll"),
        Ok(PathBuf::from(r"c:\tmp1\pdu.dll"))
    );
    assert_eq!(
        uri_to_path(r"file:///c:\tmp1\pdu.dll"),
        Ok(PathBuf::from(r"c:\tmp1\pdu.dll"))
    );
    assert_eq!(uri_to_path("file:/dir/x.dll"), Err(UriFault::Relative));
    assert_eq!(uri_to_path("file:///c:"), Err(UriFault::Relative));
    assert_eq!(uri_to_path(r"file:///a/..\b/x.dll"), Err(UriFault::Invalid));
    assert_eq!(
        uri_to_path(r"file:///c:/a/..\x.dll"),
        Err(UriFault::Invalid)
    );
    let xml = root(&entry_with("file:/dir/x.dll", ""));
    assert!(matches!(
        only_invalid(&xml).error,
        EntryError::RelativePath {
            element: "LIBRARY_FILE",
            ..
        }
    ));
}

#[cfg(not(windows))]
#[test]
fn unix_forms() {
    assert_eq!(
        uri_to_path("file:/opt/lib.so"),
        Ok(PathBuf::from("/opt/lib.so"))
    );
    assert_eq!(
        uri_to_path("file:///opt/a%20b/lib.so"),
        Ok(PathBuf::from("/opt/a b/lib.so"))
    );
    assert_eq!(uri_to_path("file:/c:/tmp1/pdu.dll"), Err(UriFault::Invalid));
}

#[test]
fn file_uri_scheme_is_case_insensitive_and_required() {
    assert!(uri_to_path(&uri("a/lib.so").replacen("file", "FILE", 1)).is_ok());
    assert_eq!(uri_to_path("http://h/opt/x"), Err(UriFault::Invalid));
    assert_eq!(uri_to_path(""), Err(UriFault::Invalid));
}

#[test]
fn entries_must_be_direct_children_of_the_root_element() {
    let xml = root(&format!(
        "<WRAPPER>{}</WRAPPER>{}",
        entry("NESTED", "n/lib.so"),
        entry("DIRECT", "d/lib.so")
    ));
    let parsed = parse_root_file(&xml).unwrap();
    assert!(parsed.invalid.is_empty());
    assert_eq!(parsed.implementations.len(), 1);
    assert_eq!(parsed.implementations[0].short_name, "DIRECT");
    // An entry inside an entry is not an entry either.
    let xml = root(&format!(
        "<MVCI_PDU_API><SHORT_NAME>OUTER</SHORT_NAME><LIBRARY_FILE URI=\"{}\"/>{}\
         </MVCI_PDU_API>",
        uri("o/lib.so"),
        entry("INNER", "i/lib.so")
    ));
    let parsed = parse_root_file(&xml).unwrap();
    assert_eq!(parsed.implementations.len(), 1);
    assert_eq!(parsed.implementations[0].short_name, "OUTER");
}

#[test]
fn a_wrong_document_element_is_not_a_root_file() {
    let xml = format!(
        "<MVCI_PDU_API>{}</MVCI_PDU_API>",
        "<SHORT_NAME>X</SHORT_NAME>"
    );
    match parse_root_file(&xml).unwrap_err() {
        ResolveError::NotARootFile { path, element } => {
            assert_eq!(path, PathBuf::from("<memory>"));
            assert_eq!(element, "MVCI_PDU_API");
        }
        other => panic!("unexpected {other:?}"),
    }
    let (_dir, file) = write_root("<OTHER/>");
    let err = resolve_in_root_file(&file, "X").unwrap_err();
    assert!(matches!(&err, ResolveError::NotARootFile { path, .. } if *path == file));
    assert!(err.to_string().contains("pdu_api_root.xml"));
}

#[test]
fn the_version_attribute_and_a_default_namespace_do_not_matter() {
    let xml = format!(
        "<MVCI_PDU_API_ROOT xmlns=\"urn:example:root\" Version=\"99\">{}</MVCI_PDU_API_ROOT>",
        entry("X", "x/lib.so")
    );
    assert_eq!(parse_root_file(&xml).unwrap().implementations.len(), 1);
}

#[test]
fn a_duplicated_child_makes_the_entry_unusable() {
    let lib = format!("<LIBRARY_FILE URI=\"{}\"/>", uri("x/lib.so"));
    for element in KNOWN_CHILDREN {
        let one = match element {
            "LIBRARY_FILE" => lib.clone(),
            "SHORT_NAME" => "<SHORT_NAME>X</SHORT_NAME>".to_owned(),
            e if e.ends_with("_FILE") => format!("<{e} URI=\"{}\"/>", uri("x/f.xml")),
            e => format!("<{e}>text</{e}>"),
        };
        let xml = root(&format!(
            "<MVCI_PDU_API><SHORT_NAME>X</SHORT_NAME>{lib}{one}{one}</MVCI_PDU_API>"
        ));
        let mut parsed = parse_root_file(&xml).unwrap();
        // The SHORT_NAME and LIBRARY_FILE cases have the element three times; still one entry.
        assert_eq!(parsed.invalid.len(), 1, "{element}");
        let entry = parsed.invalid.remove(0);
        assert_eq!(entry.short_name.as_deref(), Some("X"), "{element}");
        assert!(
            matches!(entry.error, EntryError::Duplicate { element: e } if e == element),
            "{element}: {:?}",
            entry.error
        );
    }
}

#[test]
fn resolving_a_name_with_a_duplicated_child_is_an_invalid_entry() {
    let xml = root(
        "<MVCI_PDU_API><SHORT_NAME>FIRST</SHORT_NAME><SHORT_NAME>SECOND</SHORT_NAME>\
         </MVCI_PDU_API>",
    );
    let (_dir, file) = write_root(&xml);
    assert!(matches!(
        resolve_in_root_file(&file, "FIRST"),
        Err(ResolveError::InvalidEntry {
            source: EntryError::Duplicate {
                element: "SHORT_NAME"
            },
            ..
        })
    ));
    // The second name does not identify the entry.
    assert!(matches!(
        resolve_in_root_file(&file, "SECOND"),
        Err(ResolveError::NotFound {
            skipped_invalid: 1,
            ..
        })
    ));
}

#[test]
fn unknown_children_are_ignored() {
    let xml = root(&entry_with(
        &uri("x/lib.so"),
        "<EXTRA/><EXTRA/><VENDOR_DATA><LIBRARY_FILE URI=\"http://x\"/></VENDOR_DATA>",
    ));
    assert_eq!(parse_root_file(&xml).unwrap().implementations.len(), 1);
}

#[test]
fn only_the_unnamespaced_uri_attribute_counts() {
    let only_prefixed = root(&format!(
        "<MVCI_PDU_API xmlns:p=\"urn:example:p\"><SHORT_NAME>X</SHORT_NAME>\
         <LIBRARY_FILE p:URI=\"{}\"/></MVCI_PDU_API>",
        uri("x/lib.so")
    ));
    assert_eq!(
        only_invalid(&only_prefixed).error,
        EntryError::MissingLibrary
    );

    let both = root(&format!(
        "<MVCI_PDU_API xmlns:p=\"urn:example:p\"><SHORT_NAME>X</SHORT_NAME>\
         <LIBRARY_FILE p:URI=\"http://wrong/x\" URI=\"{}\"/></MVCI_PDU_API>",
        uri("x/lib.so")
    ));
    assert_eq!(
        parse_root_file(&both).unwrap().implementations[0].library_file,
        path("x/lib.so")
    );
}

#[test]
fn short_name_text_is_joined_across_comments_and_cdata() {
    for (inner, expected) in [
        ("AB<!-- note -->CD", "ABCD"),
        ("  AB<![CDATA[CD]]>EF ", "ABCDEF"),
        ("<![CDATA[ONLY]]>", "ONLY"),
    ] {
        let xml = root(&format!(
            "<MVCI_PDU_API><SHORT_NAME>{inner}</SHORT_NAME>\
             <LIBRARY_FILE URI=\"{}\"/></MVCI_PDU_API>",
            uri("x/lib.so")
        ));
        let parsed = parse_root_file(&xml).unwrap();
        assert_eq!(parsed.implementations[0].short_name, expected, "{inner}");
    }
}

#[test]
fn an_element_inside_short_name_makes_the_entry_unusable() {
    let xml = root(&format!(
        "<MVCI_PDU_API><SHORT_NAME>AB<b>x</b>CD</SHORT_NAME>\
         <LIBRARY_FILE URI=\"{}\"/></MVCI_PDU_API>",
        uri("x/lib.so")
    ));
    let entry = only_invalid(&xml);
    assert_eq!(entry.error, EntryError::NestedShortName);
    assert_eq!(entry.short_name.as_deref(), Some("ABCD"));
}

fn utf16_bytes(text: &str, big_endian: bool, bom: bool) -> Vec<u8> {
    let mut units: Vec<u16> = Vec::new();
    if bom {
        units.push(0xFEFF);
    }
    units.extend(text.encode_utf16());
    units
        .into_iter()
        .flat_map(|u| {
            if big_endian {
                u.to_be_bytes()
            } else {
                u.to_le_bytes()
            }
        })
        .collect()
}

fn write_bytes(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pdu_api_root.xml");
    fs::write(&file, bytes).unwrap();
    (dir, file)
}

#[test]
fn utf16_and_utf8_with_a_byte_order_mark_are_read() {
    // A non-ASCII description shows the characters survive the decoding.
    let xml = root(&format!(
        "<MVCI_PDU_API><SHORT_NAME>V</SHORT_NAME><DESCRIPTION>Caf\u{e9} \u{1F408}</DESCRIPTION>\
         <LIBRARY_FILE URI=\"{}\"/></MVCI_PDU_API>",
        uri("v/lib.so")
    ));
    let utf8_bom: Vec<u8> = [&[0xEF, 0xBB, 0xBF][..], xml.as_bytes()].concat();
    for (label, bytes) in [
        ("utf-16le", utf16_bytes(&xml, false, true)),
        ("utf-16be", utf16_bytes(&xml, true, true)),
        ("utf-8 bom", utf8_bom),
        ("utf-8", xml.clone().into_bytes()),
    ] {
        let (_dir, file) = write_bytes(&bytes);
        let resolved = resolve_in_root_file(&file, "V").unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(
            resolved.implementation.description.as_deref(),
            Some("Caf\u{e9} \u{1F408}"),
            "{label}"
        );
    }
}

#[test]
fn other_encodings_are_refused() {
    let latin1 = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\
                   <MVCI_PDU_API_ROOT><MVCI_PDU_API><DESCRIPTION>Caf\xe9</DESCRIPTION>\
                   </MVCI_PDU_API></MVCI_PDU_API_ROOT>";
    let mut odd = utf16_bytes("<a/>", false, true);
    odd.pop();
    let mut lone_surrogate = vec![0xFF, 0xFE];
    lone_surrogate.extend(0xD800u16.to_le_bytes());
    for (label, bytes) in [
        ("latin-1", latin1.to_vec()),
        ("odd utf-16", odd),
        ("lone surrogate", lone_surrogate),
    ] {
        let (_dir, file) = write_bytes(&bytes);
        let err = resolve_in_root_file(&file, "X").unwrap_err();
        assert!(
            matches!(&err, ResolveError::Encoding { path } if *path == file),
            "{label}: {err:?}"
        );
        assert!(err.to_string().contains("pdu_api_root.xml"), "{label}");
    }
}

#[test]
fn the_size_limit_is_exact() {
    let base = root(&entry("X", "x/lib.so"));
    let pad = |total: u64| format!("{base}{}", " ".repeat(total as usize - base.len()));

    let (_dir, file) = write_root(&pad(MAX_ROOT_FILE_SIZE));
    assert_eq!(fs::metadata(&file).unwrap().len(), MAX_ROOT_FILE_SIZE);
    assert!(resolve_in_root_file(&file, "X").is_ok());

    let (_dir, file) = write_root(&pad(MAX_ROOT_FILE_SIZE + 1));
    assert_eq!(fs::metadata(&file).unwrap().len(), MAX_ROOT_FILE_SIZE + 1);
    assert!(matches!(
        resolve_in_root_file(&file, "X"),
        Err(ResolveError::TooLarge { .. })
    ));
}

#[test]
fn a_root_file_far_over_the_size_limit_is_refused() {
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
fn the_root_file_path_is_the_configured_one_in_either_view() {
    for view in [RegistryView::Wow64_32, RegistryView::Wow64_64] {
        assert_eq!(
            root_file_path(view).unwrap(),
            Some(vci_service_config::pdu_api_root_file())
        );
    }
}

fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| {
        vars.iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| (*v).to_owned())
    }
}

#[test]
fn expand_env_replaces_known_variables() {
    let env = env_of(&[("ROOT", r"C:\Data"), ("SUB", "pdu")]);
    assert_eq!(
        expand_env(r"%ROOT%\%SUB%\r.xml", &env),
        r"C:\Data\pdu\r.xml"
    );
    assert_eq!(expand_env("no variables", &env), "no variables");
    assert_eq!(expand_env("", &env), "");
}

#[test]
fn expand_env_leaves_unknown_and_unmatched_percents() {
    let env = env_of(&[("ROOT", "R")]);
    assert_eq!(expand_env(r"%X%\r.xml", &env), r"%X%\r.xml");
    assert_eq!(expand_env("100%", &env), "100%");
    assert_eq!(expand_env("%ROOT", &env), "%ROOT");
    assert_eq!(expand_env("a%%b", &env), "a%%b");
    assert_eq!(expand_env("%%", &env), "%%");
    // An unknown name does not swallow a known one that follows.
    assert_eq!(expand_env("%X%ROOT%", &env), "%XR");
    assert_eq!(expand_env("50% of %ROOT%", &env), "50% of R");
}

#[test]
fn expansion_does_not_rescan_values() {
    let env = env_of(&[("A", "%B%"), ("B", "b")]);
    assert_eq!(expand_env("%A%", &env), "%B%");
}

#[test]
fn the_view_selects_the_program_files_variables() {
    let env = env_of(&[
        ("ProgramFiles", r"C:\Program Files"),
        ("ProgramFiles(x86)", r"C:\Program Files (x86)"),
        ("ProgramW6432", r"C:\Program Files"),
        ("CommonProgramFiles", r"C:\Program Files\Common Files"),
        (
            "CommonProgramFiles(x86)",
            r"C:\Program Files (x86)\Common Files",
        ),
        ("CommonProgramW6432", r"C:\Program Files\Common Files"),
        ("Other", "o"),
    ]);
    let lookup = |view, name: &str| view_variable(view, name, &env);
    assert_eq!(
        lookup(RegistryView::Wow64_32, "ProgramFiles").as_deref(),
        Some(r"C:\Program Files (x86)")
    );
    assert_eq!(
        lookup(RegistryView::Wow64_64, "ProgramFiles").as_deref(),
        Some(r"C:\Program Files")
    );
    assert_eq!(
        lookup(RegistryView::Wow64_32, "COMMONPROGRAMFILES").as_deref(),
        Some(r"C:\Program Files (x86)\Common Files")
    );
    assert_eq!(
        lookup(RegistryView::Wow64_64, "CommonProgramFiles").as_deref(),
        Some(r"C:\Program Files\Common Files")
    );
    assert_eq!(
        lookup(RegistryView::Wow64_64, "Other").as_deref(),
        Some("o")
    );
    assert_eq!(lookup(RegistryView::Wow64_64, "Absent"), None);
}

#[test]
fn the_view_falls_back_to_the_plain_names() {
    // 32-bit Windows has no `(x86)` or `W6432` variables.
    let env = env_of(&[
        ("ProgramFiles", r"C:\Program Files"),
        ("CommonProgramFiles", r"C:\Program Files\Common Files"),
    ]);
    for view in [RegistryView::Wow64_32, RegistryView::Wow64_64] {
        assert_eq!(
            view_variable(view, "ProgramFiles", &env).as_deref(),
            Some(r"C:\Program Files")
        );
        assert_eq!(
            view_variable(view, "CommonProgramFiles", &env).as_deref(),
            Some(r"C:\Program Files\Common Files")
        );
    }
}

/// UTF-16LE with a terminating NUL, as the registry stores a string.
fn registry_string(text: &str) -> Vec<u8> {
    let mut bytes = utf16_bytes(text, false, false);
    bytes.extend([0, 0]);
    bytes
}

#[test]
fn a_registry_value_expands_only_when_asked() {
    let env = env_of(&[
        ("ProgramFiles", r"C:\PF64"),
        ("ProgramFiles(x86)", r"C:\PF32"),
    ]);
    let raw = registry_string(r"  %ProgramFiles%\Vendor\root.xml  ");
    let literal = root_file_from_value(&raw, false, RegistryView::Wow64_64, &env).unwrap();
    assert_eq!(
        literal,
        Some(PathBuf::from(r"%ProgramFiles%\Vendor\root.xml"))
    );
    let expanded = root_file_from_value(&raw, true, RegistryView::Wow64_32, &env).unwrap();
    assert_eq!(expanded, Some(PathBuf::from(r"C:\PF32\Vendor\root.xml")));
}

#[test]
fn a_registry_value_is_decoded_and_checked() {
    let env = env_of(&[]);
    let view = RegistryView::Wow64_64;
    // Several terminating NULs are stripped.
    let mut padded = registry_string("a.xml");
    padded.extend([0, 0, 0, 0]);
    assert_eq!(
        root_file_from_value(&padded, false, view, &env).unwrap(),
        Some(PathBuf::from("a.xml"))
    );
    assert_eq!(root_file_from_value(&[], true, view, &env).unwrap(), None);
    assert_eq!(
        root_file_from_value(&registry_string("   "), false, view, &env).unwrap(),
        None
    );
    assert!(root_file_from_value(&[0x41], false, view, &env).is_err());
    assert!(root_file_from_value(&registry_string("a\0b"), false, view, &env).is_err());
    // An expansion that comes out blank is no path either.
    let env = env_of(&[("E", "")]);
    assert_eq!(
        root_file_from_value(&registry_string("%E%"), true, view, &env).unwrap(),
        None
    );
}

#[cfg(windows)]
mod registry {
    use winreg::{
        RegKey, RegValue,
        enums::{HKEY_CURRENT_USER, REG_EXPAND_SZ, REG_SZ},
    };

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

        fn set_raw(&self, text: &str, expand: bool) {
            let value = RegValue {
                bytes: registry_string(text),
                vtype: if expand { REG_EXPAND_SZ } else { REG_SZ },
            };
            self.0.set_raw_value("Root File", &value).unwrap();
        }
    }

    impl Drop for TempKey {
        fn drop(&mut self) {
            let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(&self.1);
        }
    }

    const VIEW: RegistryView = RegistryView::Wow64_64;

    #[test]
    fn root_file_is_read_and_trimmed() {
        let temp = TempKey::new("value");
        temp.0
            .set_value("Root File", &r"  C:\ProgramData\pdu_api_root.xml  ")
            .unwrap();
        assert_eq!(
            root_file_from_key(&temp.0, VIEW).unwrap(),
            Some(PathBuf::from(r"C:\ProgramData\pdu_api_root.xml"))
        );
    }

    #[test]
    fn a_missing_or_blank_value_is_no_root_file() {
        let temp = TempKey::new("missing");
        assert_eq!(root_file_from_key(&temp.0, VIEW).unwrap(), None);
        temp.0.set_value("Root File", &"   ").unwrap();
        assert_eq!(root_file_from_key(&temp.0, VIEW).unwrap(), None);
    }

    #[test]
    fn reg_sz_is_taken_literally() {
        let temp = TempKey::new("sz");
        temp.set_raw(r"%SystemRoot%\pdu_api_root.xml", false);
        assert_eq!(
            root_file_from_key(&temp.0, VIEW).unwrap(),
            Some(PathBuf::from(r"%SystemRoot%\pdu_api_root.xml"))
        );
    }

    #[test]
    fn reg_expand_sz_is_expanded() {
        let temp = TempKey::new("expand");
        temp.set_raw(r"%SystemRoot%\pdu_api_root.xml", true);
        let system_root = std::env::var("SystemRoot").unwrap();
        assert_eq!(
            root_file_from_key(&temp.0, VIEW).unwrap(),
            Some(PathBuf::from(format!(r"{system_root}\pdu_api_root.xml")))
        );
    }

    #[test]
    fn reg_expand_sz_uses_the_views_program_files() {
        let temp = TempKey::new("views");
        temp.set_raw(r"%ProgramFiles%\Vendor\root.xml", true);
        let var = |name: &str| std::env::var(name).ok();
        for (view, specific) in [
            (RegistryView::Wow64_32, "ProgramFiles(x86)"),
            (RegistryView::Wow64_64, "ProgramW6432"),
        ] {
            let base = var(specific).or_else(|| var("ProgramFiles")).unwrap();
            assert_eq!(
                root_file_from_key(&temp.0, view).unwrap(),
                Some(PathBuf::from(format!(r"{base}\Vendor\root.xml"))),
                "{view:?}"
            );
        }
    }

    #[test]
    fn a_value_that_is_not_a_string_is_an_error() {
        let temp = TempKey::new("dword");
        temp.0.set_value("Root File", &1u32).unwrap();
        assert!(matches!(
            root_file_from_key(&temp.0, VIEW),
            Err(ResolveError::Registry(_))
        ));
    }

    #[test]
    fn the_root_file_is_found_in_both_views_without_error() {
        // The key normally does not exist on a build machine; the lookup must still succeed.
        for view in [RegistryView::Wow64_32, RegistryView::Wow64_64] {
            root_file_path(view).unwrap();
        }
    }
}

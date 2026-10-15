use std::path::PathBuf;

use diag_ir::declaration::{EcuDocumentT, load_ecu_document};

use super::*;
use crate::tables::{read_directory, read_files, resolve};

mod dtcs_and_flash;
mod services;

/// The example definition's files, by name.
fn example() -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/example-ecu");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("example directory")
        .map(|entry| {
            let entry = entry.expect("entry");
            let name = entry.file_name().into_string().expect("UTF-8 name");
            (name, std::fs::read(entry.path()).expect("read"))
        })
        .collect();
    files.sort();
    files
}

/// The example with `file` replaced by `contents` (added if absent).
fn with(mut files: Vec<(String, Vec<u8>)>, file: &str, contents: &str) -> Vec<(String, Vec<u8>)> {
    files.retain(|(name, _)| name != file);
    files.push((file.to_owned(), contents.as_bytes().to_vec()));
    files
}

fn convert(files: Vec<(String, Vec<u8>)>) -> Converted {
    let tables = read_files(files).unwrap_or_else(|e| panic!("does not read: {e:?}"));
    to_ir(&resolve(tables).unwrap_or_else(|e| panic!("does not resolve: {e:?}")))
}

/// The documents, read back through the loader the agent uses.
fn documents(converted: &Converted) -> Vec<EcuDocumentT> {
    converted
        .documents
        .iter()
        .map(|document| {
            load_ecu_document(&document.bytes)
                .expect("the document loads")
                .unpack()
        })
        .collect()
}

fn variant(document: &EcuDocumentT) -> &EcuVariantT {
    document.variant.as_deref().expect("a variant")
}

/// Each protocol as (name, baud rate, tx, rx, functional, extended, its ComParams as
/// (name, value, unit id)).
type ProtocolView = (String, u32, u32, u32, u32, bool, Vec<(String, i64, u32)>);

fn protocols(document: &EcuDocumentT) -> Vec<ProtocolView> {
    variant(document)
        .protocols
        .iter()
        .flatten()
        .map(|p| {
            let params = p
                .params
                .iter()
                .flatten()
                .map(|c| (c.short_name.clone().expect("name"), c.value, c.unit_id))
                .collect();
            (
                p.short_name.clone().expect("name"),
                p.baud_rate,
                p.tx_id,
                p.rx_id,
                p.functional_id,
                p.extended_addressing,
                params,
            )
        })
        .collect()
}

#[test]
fn the_example_converts_into_one_document_per_variant() {
    let files = example();
    let digest = source_digest(files.iter().map(|(n, c)| (n.as_str(), c.as_slice())));
    let converted = convert(files);
    let ids: Vec<(u32, &str)> = converted
        .documents
        .iter()
        .map(|d| (d.id, d.name.as_str()))
        .collect();
    assert_eq!(ids, [(1, "exampleEcuV1"), (2, "exampleEcuV2")]);
    let documents = documents(&converted);
    let expected_versions: [&[&str]; 2] = [&["1.0.0", "1.0.1"], &["2.0.0"]];
    for ((document, converted), versions) in documents
        .iter()
        .zip(&converted.documents)
        .zip(expected_versions)
    {
        assert_eq!(document.schema_version, IR_SCHEMA_VERSION);
        assert_eq!(document.source_format.as_deref(), Some(SOURCE_FORMAT));
        assert_eq!(document.source_digest.as_deref(), Some(digest.as_str()));
        assert!(document.procedure.is_none());
        let variant = variant(document);
        assert_eq!(variant.id, converted.id);
        assert_eq!(variant.short_name.as_deref(), Some(converted.name.as_str()));
        assert_eq!(
            variant.part_numbers.as_deref(),
            Some(&["NGR-EX-0001".to_owned()][..])
        );
        let versions: Vec<String> = versions.iter().map(|v| (*v).to_owned()).collect();
        assert_eq!(variant.sw_versions.as_deref(), Some(versions.as_slice()));
        assert_eq!(
            protocols(document),
            [(
                "ISO15765".to_owned(),
                500_000,
                0x7E0,
                0x7E8,
                0x7DF,
                false,
                vec![
                    ("CP_BlockSize".to_owned(), 0, 0),
                    ("CP_StMin".to_owned(), 0, 0),
                ],
            )]
        );
    }
}

#[test]
fn the_conversion_is_deterministic() {
    let first = convert(example());
    let mut reversed = example();
    reversed.reverse();
    assert_eq!(first, convert(reversed));
}

#[test]
fn the_catalogs_are_sorted_by_id() {
    // The example's texts in reverse order; other tables refer to them by key.
    let texts = example()
        .into_iter()
        .find(|(name, _)| name == "texts.csv")
        .map(|(_, contents)| String::from_utf8(contents).expect("UTF-8"))
        .expect("texts.csv");
    let mut lines: Vec<&str> = texts.lines().collect();
    lines[1..].reverse();
    let files = with(example(), "texts.csv", &(lines.join("\n") + "\n"));
    let files = with(
        files,
        "units.csv",
        "key,id,symbol\nvolt,9,V\nmillisecond,2,\n",
    );
    let converted = convert(files);
    let ids: Vec<u32> = converted.texts.iter().map(|e| e.id).collect();
    assert_eq!(ids, [1, 2, 3, 4, 5, 6, 10, 11, 12, 13, 14, 20, 21]);
    assert_eq!(
        (converted.texts[0].id, converted.texts[0].text.as_str()),
        (1, "Default session")
    );
    let units: Vec<(u32, &str)> = converted
        .units
        .iter()
        .map(|e| (e.id, e.text.as_str()))
        .collect();
    assert_eq!(units, [(2, ""), (9, "V")]);
}

#[test]
fn each_variant_gets_its_own_protocols_and_comparams() {
    let files = with(
        example(),
        "protocols.csv",
        "name,baud_rate,tx_id,rx_id,functional_id,extended_addressing,variants\n\
         ISO15765,500000,0x7E0,0x7E8,0x7DF,,\n\
         ISO15765_ext,250000,0x7E1,0x7E9,,yes,exampleEcuV2\n",
    );
    let files = with(
        files,
        "comparams.csv",
        "protocol,name,value,unit,variants\n\
         ISO15765_ext,CP_BlockSize,8,,exampleEcuV2\n\
         ISO15765,CP_P2Max,50,millisecond,\n\
         ISO15765,CP_StMin,-1,,exampleEcuV1\n",
    );
    let documents = documents(&convert(files));
    let base = |params: Vec<(String, i64, u32)>| {
        (
            "ISO15765".to_owned(),
            500_000,
            0x7E0,
            0x7E8,
            0x7DF,
            false,
            params,
        )
    };
    assert_eq!(
        protocols(&documents[0]),
        [base(vec![
            ("CP_P2Max".to_owned(), 50, 2),
            ("CP_StMin".to_owned(), -1, 0),
        ])]
    );
    assert_eq!(
        protocols(&documents[1]),
        [
            base(vec![("CP_P2Max".to_owned(), 50, 2)]),
            (
                "ISO15765_ext".to_owned(),
                250_000,
                0x7E1,
                0x7E9,
                0,
                true,
                vec![("CP_BlockSize".to_owned(), 8, 0)],
            ),
        ]
    );
}

#[test]
fn comparams_follow_the_protocol_of_their_own_variant() {
    // One protocol name with a different row in each variant.
    let files = with(
        example(),
        "protocols.csv",
        "name,baud_rate,tx_id,rx_id,variants\n\
         ISO15765,500000,0x7E0,0x7E8,exampleEcuV1\n\
         ISO15765,250000,0x7E1,0x7E9,exampleEcuV2\n",
    );
    let files = with(
        files,
        "comparams.csv",
        "protocol,name,value\nISO15765,CP_StMin,5\n",
    );
    let documents = documents(&convert(files));
    let protocol = |baud, tx, rx| {
        (
            "ISO15765".to_owned(),
            baud,
            tx,
            rx,
            0,
            false,
            vec![("CP_StMin".to_owned(), 5, 0)],
        )
    };
    assert_eq!(protocols(&documents[0]), [protocol(500_000, 0x7E0, 0x7E8)]);
    assert_eq!(protocols(&documents[1]), [protocol(250_000, 0x7E1, 0x7E9)]);
}

#[test]
fn the_source_digest_follows_section_6() {
    // SHA-256 of nothing.
    assert_eq!(
        source_digest([]),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    // a.csv, a zero byte, 0 as 8 bytes; then b.csv, a zero byte, 2 as 8 bytes, "x\n".
    let expected = "4787356399acb32fc07f6041ed9a87aaca1d560f853c13d4f18ada2864bd03c1";
    assert_eq!(
        source_digest([("b.csv", &b"x\n"[..]), ("a.csv", &b""[..])]),
        expected
    );
    assert_eq!(
        source_digest([("a.csv", &b""[..]), ("b.csv", &b"x\n"[..])]),
        expected
    );
    // A repeated name does not make the digest depend on the order of the files.
    assert_eq!(
        source_digest([("a.csv", &b"1"[..]), ("a.csv", &b"2"[..])]),
        source_digest([("a.csv", &b"2"[..]), ("a.csv", &b"1"[..])])
    );
}

#[test]
fn the_source_digest_covers_the_table_files_as_stored() {
    let digest = |files: Vec<(String, Vec<u8>)>| {
        let converted = convert(files);
        documents(&converted)[0]
            .source_digest
            .clone()
            .expect("a digest")
    };
    let plain = digest(example());
    // Files that are not tables are not read.
    let mut with_readme = example();
    with_readme.push(("README".to_owned(), b"notes".to_vec()));
    assert_eq!(digest(with_readme), plain);
    // Nor is a procedure file the document does not carry, and no document carries one yet.
    let mut with_procedure = example();
    with_procedure.push(("main.star".to_owned(), b"def main():\n    pass\n".to_vec()));
    assert_eq!(digest(with_procedure), plain);
    // Reading the directory covers the same bytes.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/example-ecu");
    let from_dir = to_ir(&resolve(read_directory(&dir).expect("reads")).expect("resolves"));
    assert_eq!(from_dir, convert(example()));
    // Line ends are part of the bytes. The checkout may already use CRLF (Git on Windows), so
    // the line ends of one table are switched to the other kind.
    let switched: Vec<_> = example()
        .into_iter()
        .map(|(name, contents)| {
            if name == "variants.csv" {
                let text = String::from_utf8(contents).expect("UTF-8");
                let text = if text.contains("\r\n") {
                    text.replace("\r\n", "\n")
                } else {
                    text.replace('\n', "\r\n")
                };
                (name, text.into_bytes())
            } else {
                (name, contents)
            }
        })
        .collect();
    assert_ne!(digest(switched), plain);
}

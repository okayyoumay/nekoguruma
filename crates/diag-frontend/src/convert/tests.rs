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
    let definition = resolve(tables).unwrap_or_else(|e| panic!("does not resolve: {e:?}"));
    let procedures =
        lower_procedures(&definition).unwrap_or_else(|e| panic!("does not lower: {e:?}"));
    to_ir(&definition, &procedures)
}

/// The procedures of `files`, or why they were rejected.
fn procedures(files: Vec<(String, Vec<u8>)>) -> Result<Vec<Procedure>, ProceduresRejected> {
    lower_procedures(&resolve(read_files(files).expect("reads")).expect("resolves"))
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
    assert!(converted.documents.iter().all(|d| d.procedure.is_none()));
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
    // With procedures, given in either order.
    let procedures = with(with(example(), "a.star", LOG), "b.star", FAIL);
    let mut reversed = procedures.clone();
    reversed.reverse();
    assert_eq!(convert(procedures), convert(reversed));
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
    // A procedure file is covered by the documents that carry it (ADR-307).
    let mut with_procedure = example();
    with_procedure.push(("main.star".to_owned(), b"def main():\n    pass\n".to_vec()));
    let tables_and_procedure = source_digest(
        with_procedure
            .iter()
            .map(|(n, c)| (n.as_str(), c.as_slice())),
    );
    assert_eq!(digest(with_procedure), tables_and_procedure);
    assert_ne!(tables_and_procedure, plain);
    // Reading the directory covers the same bytes.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/example-ecu");
    let definition = resolve(read_directory(&dir).expect("reads")).expect("resolves");
    let procedures = lower_procedures(&definition).expect("lowers");
    let from_dir = to_ir(&definition, &procedures);
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

// ---------------------------------------------------------------- Procedures (ADR-307)

const LOG: &str = "def main():\n    diag.log(1, \"a\")\n";
const FAIL: &str = "def main():\n    diag.fail(\"STOP\", \"\")\n";

#[test]
fn every_procedure_goes_with_every_variant() {
    let files = with(with(example(), "b.star", FAIL), "a.star", LOG);
    let converted = convert(files.clone());
    let keys: Vec<(&str, Option<&str>)> = converted
        .documents
        .iter()
        .map(|d| (d.name.as_str(), d.procedure.as_deref()))
        .collect();
    assert_eq!(
        keys,
        [
            ("exampleEcuV1", Some("a.star")),
            ("exampleEcuV1", Some("b.star")),
            ("exampleEcuV2", Some("a.star")),
            ("exampleEcuV2", Some("b.star")),
        ]
    );
    let tables: Vec<(&str, &[u8])> = files
        .iter()
        .filter(|(n, _)| n.ends_with(".csv"))
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    for converted in &converted.documents {
        let document = load_ecu_document(&converted.bytes).expect("the document loads");
        let file = converted.procedure.as_deref().expect("a procedure");
        let source = if file == "a.star" { LOG } else { FAIL };
        // Each document's digest covers the tables and its own procedure file only.
        let expected = source_digest(tables.iter().copied().chain([(file, source.as_bytes())]));
        assert_eq!(document.source_digest(), Some(expected.as_str()));
        // The procedure is the program lowered for the document's variant, as postcard bytes.
        let bytes = document.procedure().expect("a program");
        let program: diag_ir::Program = postcard::from_bytes(bytes.bytes()).expect("a program");
        let checked = crate::star::check(file, source).expect("checks");
        let lowered = crate::star::lower(file, &checked, document.variant().expect("a variant"))
            .expect("lowers")
            .program;
        assert_eq!(
            postcard::to_allocvec(&program).expect("encodes"),
            postcard::to_allocvec(&lowered).expect("encodes")
        );
    }
}

#[test]
fn every_procedure_file_reports_its_errors() {
    // Two files fail, one to check and one because it is not UTF-8; both are reported, and a
    // file that lowers does not hide them.
    let mut files = with(
        with(example(), "ok.star", LOG),
        "bad.star",
        "def f():\n    pass\n",
    );
    files.push((
        "latin1.star".to_owned(),
        b"def main():\n    pass # \xe9\n".to_vec(),
    ));
    let errors = procedures(files).expect_err("two files fail");
    let found: Vec<(&str, crate::star::StarCode, u32)> = errors
        .errors
        .iter()
        .map(|e| (e.file.as_str(), e.code, e.line))
        .collect();
    assert_eq!(
        found,
        [
            ("bad.star", crate::star::StarCode::NoMain, 1),
            ("latin1.star", crate::star::StarCode::SyntaxError, 2),
        ]
    );
}

#[test]
fn a_rejected_procedure_keeps_the_warnings_of_the_others() {
    // `a.star` lowers with the default-limit warning; `b.star` has no `main`.
    let a = "def f(n):\n    for i in range(n):\n        pass\n\ndef main():\n    f(1)\n";
    let files = with(
        with(example(), "a.star", a),
        "b.star",
        "def f():\n    pass\n",
    );
    let rejected = procedures(files).expect_err("b.star fails");
    assert_eq!(rejected.errors.len(), 1);
    let warnings: Vec<(&str, crate::star::StarCode)> = rejected
        .warnings
        .iter()
        .map(|w| (w.file.as_str(), w.code))
        .collect();
    assert_eq!(
        warnings,
        [("a.star", crate::star::StarCode::LoopLimitDefaulted)]
    );
}

#[test]
fn a_file_too_large_is_reported_before_its_encoding() {
    let mut files = example();
    let mut big = vec![b'#'; crate::star::MAX_SOURCE_BYTES + 1];
    big[0] = 0xFF;
    files.push(("big.star".to_owned(), big));
    let rejected = procedures(files).expect_err("too large");
    let codes: Vec<_> = rejected.errors.iter().map(|e| e.code).collect();
    assert_eq!(codes, [crate::star::StarCode::SourceTooLarge]);
}

#[test]
fn reading_the_directory_reads_its_procedure_files() {
    // A copy of the example with a procedure file, read from disk.
    let dir =
        std::env::temp_dir().join(format!("ngr-procedures-{}-{}", std::process::id(), line!()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temporary directory");
    let files = with(example(), "main.star", LOG);
    for (name, contents) in &files {
        std::fs::write(dir.join(name), contents).expect("written");
    }
    let definition = resolve(read_directory(&dir).expect("reads")).expect("resolves");
    let procedures = lower_procedures(&definition).expect("lowers");
    let from_dir = to_ir(&definition, &procedures);
    std::fs::remove_dir_all(&dir).expect("removed");
    assert_eq!(from_dir, convert(files));
    assert!(
        from_dir
            .documents
            .iter()
            .all(|d| d.procedure.as_deref() == Some("main.star"))
    );
}

#[test]
fn each_procedure_is_lowered_for_each_variant() {
    let procedures = procedures(with(example(), "a.star", LOG)).expect("lowers");
    let [procedure] = &procedures[..] else {
        panic!("one procedure");
    };
    // The example has two variants, each with its own program (ADR-311).
    assert_eq!(procedure.programs.len(), 2);
}

#[test]
fn an_error_every_variant_gives_is_reported_once() {
    let section = "# @section uninterruptible\ndef main():\n    pass\n";
    let rejected = procedures(with(example(), "a.star", section)).expect_err("not lowerable");
    let found: Vec<(crate::star::StarCode, u32)> =
        rejected.errors.iter().map(|e| (e.code, e.line)).collect();
    assert_eq!(found, [(crate::star::StarCode::NotLowerable, 1)]);
    assert!(!rejected.errors[0].hint.contains("(in variant"));
    assert!(!rejected.errors[0].hint.contains("(in variants"));
}

#[test]
fn an_error_only_some_variants_give_names_them() {
    let error = |line, hint: &str| StarError {
        code: crate::star::StarCode::NotLowerable,
        file: "a.star".to_owned(),
        line,
        column: 1,
        snippet: String::new(),
        hint: hint.to_owned(),
    };
    let merged = merge(
        vec![
            ("v1", vec![error(2, "both"), error(3, "v1 only")]),
            (
                "v2",
                vec![error(1, "v2 and v3"), error(2, "both"), error(2, "both")],
            ),
            ("v3", vec![error(1, "v2 and v3"), error(2, "both")]),
        ],
        3,
    );
    let hints: Vec<(u32, &str)> = merged.iter().map(|e| (e.line, e.hint.as_str())).collect();
    assert_eq!(
        hints,
        [
            (1, "v2 and v3 (in variants v2, v3)"),
            (2, "both"),
            (3, "v1 only (in variant v1)"),
        ]
    );
}

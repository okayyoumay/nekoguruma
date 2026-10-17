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
        let lowered = crate::star::lower(
            file,
            &checked,
            document.variant().expect("a variant"),
            &crate::star::RequestIndex::default(),
        )
        .expect("lowers")
        .program;
        assert_eq!(
            postcard::to_allocvec(&program).expect("encodes"),
            postcard::to_allocvec(&lowered).expect("encodes")
        );
    }
}

// ---------------------------------------------------------------- `diag.request` (ADR-313)

/// A host that records each request it is sent and answers with the positive response SID.
#[derive(Default)]
struct Recording(Sent);

#[derive(Debug, thiserror::Error)]
#[error("not sent by these tests")]
struct NotSent;

impl diag_ir::DiagHost for Recording {
    type Error = NotSent;

    fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, NotSent> {
        self.0.push((service, payload.to_vec()));
        Ok(vec![service as u8 + 0x40])
    }
    fn send_receive(&mut self, _: u32, _: &[u8]) -> Result<Vec<u8>, NotSent> {
        Err(NotSent)
    }
    fn read_dtc(&mut self, _: u8) -> Result<Vec<u8>, NotSent> {
        Err(NotSent)
    }
    fn routine_control(&mut self, _: u16, _: u8, _: &[u8]) -> Result<Vec<u8>, NotSent> {
        Err(NotSent)
    }
    fn security_access(&mut self, _: u64, _: u8, _: &[u8]) -> Result<Option<Vec<u8>>, NotSent> {
        Err(NotSent)
    }
    fn flash_transfer(&mut self, _: u32, _: &[u8]) -> Result<(), NotSent> {
        Err(NotSent)
    }
    fn wait(&mut self, _: u64, _: u32) -> Result<bool, NotSent> {
        Err(NotSent)
    }
    fn hmi_request(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, NotSent> {
        Err(NotSent)
    }
    fn record_input(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, NotSent> {
        Err(NotSent)
    }
    fn monitor_capture(&mut self, _: u32) -> Result<(), NotSent> {
        Err(NotSent)
    }
    fn log(&mut self, _: u8, _: &str) {}
}

/// The SID and operand of each request sent.
type Sent = Vec<(u16, Vec<u8>)>;

/// The requests each variant's program for `main.star` sends, by variant.
fn requests_sent(source: &str) -> Vec<(String, Sent)> {
    requests_sent_by(example(), source)
}

/// The requests each variant's program for `main.star` beside `files` sends, by variant.
fn requests_sent_by(files: Vec<(String, Vec<u8>)>, source: &str) -> Vec<(String, Sent)> {
    let converted = convert(with(files, "main.star", source));
    converted
        .documents
        .iter()
        .map(|converted| {
            let document = load_ecu_document(&converted.bytes).expect("the document loads");
            let bytes = document.procedure().expect("a program");
            let program: diag_ir::Program = postcard::from_bytes(bytes.bytes()).expect("a program");
            let mut vm = diag_ir::Vm::new(&program);
            let mut host = Recording::default();
            loop {
                match vm.step(&program, &mut host).expect("runs") {
                    diag_ir::StepOutcome::Continue => {}
                    diag_ir::StepOutcome::Finished => break,
                    other => panic!("{other:?}"),
                }
            }
            (converted.name.clone(), host.0)
        })
        .collect()
}

/// The errors `main.star` gives, as (code, hint).
fn request_errors(source: &str) -> Vec<(crate::star::StarCode, String)> {
    procedures(with(example(), "main.star", source))
        .expect_err("rejected")
        .errors
        .into_iter()
        .map(|e| (e.code, e.hint))
        .collect()
}

#[test]
fn a_request_is_selected_by_its_constants_and_encoded_from_its_fields() {
    let source = "def main():\n    \
                  diag.request(0x22, {\"did\": 0xF190})\n    \
                  diag.request(0x10, {\"sub\": 3})\n    \
                  diag.request(0x19, {\"statusMask\": 0xFF, \"sub\": 2})\n    \
                  diag.request(0x2E, {\"did\": 0xF190, \"vin\": \"WVWZZZ1JZXW000001\"})\n";
    let mut vin = vec![0xF1, 0x90];
    vin.extend_from_slice(b"WVWZZZ1JZXW000001");
    let sent = vec![
        (0x22, vec![0xF1, 0x90]),
        (0x10, vec![0x03]),
        (0x19, vec![0x02, 0xFF]),
        (0x2E, vin),
    ];
    assert_eq!(
        requests_sent(source),
        [
            ("exampleEcuV1".to_owned(), sent.clone()),
            ("exampleEcuV2".to_owned(), sent),
        ]
    );
}

#[test]
fn a_request_is_selected_by_a_bytes_constant_and_may_have_no_fields() {
    let files = services::appended(
        example(),
        "fields.csv",
        &[
            "request:ecuReset,sub,0,,uint,8,,,,,,0x01,,,",
            "request:customRead,tag,0,,bytes,16,,,,,,A1B2,,,",
            "request:customRead,mode,2,,uint,8,,,,,,,,,",
        ],
    );
    let files = services::appended(files, "services.csv", &["ecuReset,0x11", "customRead,0xBA"]);
    // `params` may also be a global constant.
    let source = "P = {\"mode\": 7, \"tag\": b\"\\xa1\\xb2\"}\n\n\
                  def main():\n    diag.request(0x11, {\"sub\": 1})\n    diag.request(0xBA, P)\n";
    let sent = vec![(0x11, vec![0x01]), (0xBA, vec![0xA1, 0xB2, 0x07])];
    assert_eq!(
        requests_sent_by(files.clone(), source),
        [
            ("exampleEcuV1".to_owned(), sent.clone()),
            ("exampleEcuV2".to_owned(), sent),
        ]
    );
    // The bytes constant is not matched by an integer of the same value.
    let errors: Vec<_> = procedures(with(
        files,
        "main.star",
        "def main():\n    diag.request(0xBA, {\"mode\": 7, \"tag\": 0xA1B2})\n",
    ))
    .expect_err("rejected")
    .errors
    .into_iter()
    .map(|e| e.code)
    .collect();
    assert_eq!(errors, [crate::star::StarCode::UnknownService]);
}

#[test]
fn a_request_no_service_matches_is_refused() {
    use crate::star::StarCode::UnknownService;
    for call in [
        // The routine service with no request fields is left to `diag.routine`.
        "diag.request(0x31, {})",
        // No service has the SID.
        "diag.request(0x85, {})",
        // No session has sub-function 4.
        "diag.request(0x10, {\"sub\": 4})",
        // A constant is missing, so it is not known which session is meant.
        "diag.request(0x10, {})",
        // A key no field of the request has.
        "diag.request(0x10, {\"sub\": 3, \"extra\": 1})",
        // A field of the request is missing.
        "diag.request(0x19, {\"sub\": 2})",
        // A constant given as the wrong type.
        "diag.request(0x22, {\"did\": b\"\\xf1\\x90\"})",
    ] {
        let errors = request_errors(&format!("def main():\n    {call}\n"));
        assert_eq!(errors.len(), 1, "{call}: {errors:?}");
        assert_eq!(errors[0].0, UnknownService, "{call}: {errors:?}");
        assert!(!errors[0].1.contains("(in variant"), "{call}: {errors:?}");
    }
}

#[test]
fn a_request_one_variant_lacks_names_the_variant() {
    // Only exampleEcuV2 declares 0xFD02.
    let errors = request_errors("def main():\n    diag.request(0x22, {\"did\": 0xFD02})\n");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].0, crate::star::StarCode::UnknownService);
    assert!(
        errors[0].1.ends_with("(in variant exampleEcuV1)"),
        "{errors:?}"
    );
}

#[test]
fn a_request_needs_its_arguments_at_ingestion() {
    use crate::star::StarCode::{ArgumentOutOfRange, NonConstantArgument, NotLowerable};
    let cases = [
        (
            "def f(n):\n    diag.request(n, {\"did\": 0xF190})\n\ndef main():\n    f(0x22)\n",
            NonConstantArgument,
        ),
        // A constant's value selects the service.
        (
            "def f(n):\n    diag.request(0x22, {\"did\": n})\n\ndef main():\n    f(0xF190)\n",
            NonConstantArgument,
        ),
        // A field's value would need encoding at run time.
        (
            "def f(n):\n    diag.request(0x19, {\"sub\": 2, \"statusMask\": n})\n\n\
             def main():\n    f(0xFF)\n",
            NotLowerable,
        ),
        (
            "def main():\n    diag.request(0x122, {})\n",
            ArgumentOutOfRange,
        ),
        // A data transfer is sent by `diag.flash_transfer`.
        (
            "def main():\n    diag.request(0x34, {})\n",
            ArgumentOutOfRange,
        ),
        (
            "def main():\n    diag.request(0x36, {})\n",
            ArgumentOutOfRange,
        ),
        (
            "def main():\n    diag.request(0x37, {})\n",
            ArgumentOutOfRange,
        ),
        // A dict past the constant size bound is reported once.
        (
            "A = b\"x\" * 600000\n\ndef main():\n    \
             diag.request(0x2E, {\"did\": 0xF190, \"vin\": A, \"x\": A})\n",
            NotLowerable,
        ),
        (
            // The VIN field is 17 bytes.
            "def main():\n    diag.request(0x2E, {\"did\": 0xF190, \"vin\": \"short\"})\n",
            ArgumentOutOfRange,
        ),
    ];
    for (source, code) in cases {
        let codes: Vec<_> = request_errors(source).into_iter().map(|e| e.0).collect();
        assert_eq!(codes, [code], "{source}");
    }
}

// ---------------------------------------------------------------- `diag.request` responses (ADR-317)

/// A host that answers every request with the next of its responses and records the messages
/// logged.
struct Answering {
    responses: std::collections::VecDeque<Vec<u8>>,
    logged: Vec<String>,
    /// Each generic send-receive's service and request.
    sent: Vec<(u32, Vec<u8>)>,
}

impl diag_ir::DiagHost for Answering {
    type Error = NotSent;

    fn service_request(&mut self, _: u16, _: &[u8]) -> Result<Vec<u8>, NotSent> {
        self.responses.pop_front().ok_or(NotSent)
    }
    fn send_receive(&mut self, service: u32, request: &[u8]) -> Result<Vec<u8>, NotSent> {
        self.sent.push((service, request.to_vec()));
        self.responses.pop_front().ok_or(NotSent)
    }
    fn read_dtc(&mut self, _: u8) -> Result<Vec<u8>, NotSent> {
        Err(NotSent)
    }
    fn routine_control(&mut self, _: u16, _: u8, _: &[u8]) -> Result<Vec<u8>, NotSent> {
        Err(NotSent)
    }
    fn security_access(&mut self, _: u64, _: u8, _: &[u8]) -> Result<Option<Vec<u8>>, NotSent> {
        Err(NotSent)
    }
    fn flash_transfer(&mut self, _: u32, _: &[u8]) -> Result<(), NotSent> {
        Err(NotSent)
    }
    fn wait(&mut self, _: u64, _: u32) -> Result<bool, NotSent> {
        Err(NotSent)
    }
    fn hmi_request(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, NotSent> {
        Err(NotSent)
    }
    fn record_input(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, NotSent> {
        Err(NotSent)
    }
    fn monitor_capture(&mut self, _: u32) -> Result<(), NotSent> {
        Err(NotSent)
    }
    fn log(&mut self, _: u8, message: &str) {
        self.logged.push(message.to_owned());
    }
}

/// Runs the first variant's program for `main.star` against `responses`, stepped with that
/// variant's declarations (ADR-316): the messages it logged, or the VM error it stopped with.
fn answered(source: &str, responses: &[&[u8]]) -> Result<Vec<String>, String> {
    answered_in(example(), source, responses)
}

/// [`answered`] for the definition `files`.
fn answered_in(
    files: Vec<(String, Vec<u8>)>,
    source: &str,
    responses: &[&[u8]],
) -> Result<Vec<String>, String> {
    run_answered(files, source, responses).map(|host| host.logged)
}

/// Runs the first variant's program for `main.star` of `files` against `responses`: the host
/// once the program finishes, or the VM error it stopped with.
fn run_answered(
    files: Vec<(String, Vec<u8>)>,
    source: &str,
    responses: &[&[u8]],
) -> Result<Answering, String> {
    let converted = convert(with(files, "main.star", source));
    let document = load_ecu_document(&converted.documents[0].bytes).expect("the document loads");
    let bytes = document.procedure().expect("a program");
    let program: diag_ir::Program = postcard::from_bytes(bytes.bytes()).expect("a program");
    let mut vm = diag_ir::Vm::new(&program);
    let mut host = Answering {
        responses: responses.iter().map(|r| r.to_vec()).collect(),
        logged: Vec::new(),
        sent: Vec::new(),
    };
    loop {
        match vm.step_with_declarations(&program, document.variant().as_ref(), &mut host) {
            Ok(diag_ir::StepOutcome::Continue) => {}
            Ok(diag_ir::StepOutcome::Finished) => return Ok(host),
            Ok(other) => panic!("{other:?}"),
            Err(e) => return Err(format!("{e:?}")),
        }
    }
}

const READ_VIN: &str = "\
def main():
    res = diag.request(0x22, {\"did\": 0xF190})
    if res[\"ok\"]:
        diag.log(1, \"positive\")
    else:
        diag.log(1, \"negative\")
    if res[\"nrc\"] == 0:
        diag.log(1, \"no nrc\")
    if res[\"nrc\"] == 0x31:
        diag.log(1, \"out of range\")
    if res[\"raw\"] == b\"\\x62\\xf1\\x90\":
        diag.log(1, \"answer\")
    if res[\"raw\"][0] == 0x7F:
        diag.log(1, \"7F\")
";

#[test]
fn a_positive_response_reads_as_ok_without_an_nrc() {
    assert_eq!(
        answered(READ_VIN, &[&[0x62, 0xF1, 0x90]]).unwrap(),
        ["positive", "no nrc", "answer"]
    );
}

#[test]
fn a_negative_response_reads_as_not_ok_with_its_nrc() {
    assert_eq!(
        answered(READ_VIN, &[&[0x7F, 0x22, 0x31]]).unwrap(),
        ["negative", "out of range", "7F"]
    );
    // Another NRC is read as itself.
    assert_eq!(
        answered(READ_VIN, &[&[0x7F, 0x22, 0x22]]).unwrap(),
        ["negative", "7F"]
    );
}

#[test]
fn an_empty_response_is_ok_and_a_short_negative_one_has_no_nrc() {
    // An empty answer has no `7F`, so it reads as positive.
    let ok = "def main():\n    if diag.request(0x22, {\"did\": 0xF190})[\"ok\"]:\n        \
              diag.log(1, \"positive\")\n";
    assert_eq!(answered(ok, &[&[]]).unwrap(), ["positive"]);
    // A negative response without its NRC byte stops the run when the NRC is read.
    let error = answered(READ_VIN, &[&[0x7F, 0x22]]).unwrap_err();
    assert!(error.contains("IndexOutOfRange"), "{error}");
}

#[test]
fn ok_and_nrc_follow_the_variant_s_declared_negative_shape() {
    let files = with(example(), "main.star", READ_VIN);
    let definition = resolve(read_files(files).expect("reads")).expect("resolves");
    let converted = to_ir(&definition, &lower_procedures(&definition).expect("lowers"));
    // Each protocol carries its application layer and that profile's shape (ADR-309 item 4).
    let mut document = documents(&converted).remove(0);
    let protocol = &mut document
        .variant
        .as_mut()
        .and_then(|v| v.protocols.as_mut())
        .expect("protocols")[0];
    assert_eq!(protocol.application_layer.as_deref(), Some("UDS"));
    let shape = protocol.negative_shape.as_mut().expect("a shape");
    assert_eq!(
        (
            shape.marker,
            shape.echoes_sid,
            shape.code_at,
            shape.pending.clone()
        ),
        (0x7F, true, 2, Some(vec![0x78]))
    );
    // Lowered against a variant that declares another shape, `ok` and `nrc` read that one.
    // A marker followed directly by its code, with no echoed service identifier.
    shape.marker = 0x7E;
    shape.echoes_sid = false;
    shape.code_at = 1;
    let bytes = encode_ecu_document(&document);
    let variant = load_ecu_document(&bytes)
        .expect("the document loads")
        .variant()
        .expect("a variant");
    let checked = crate::star::check("main.star", READ_VIN).expect("checks");
    let requests = super::services::Index::new(&definition).request_index(0);
    let program = crate::star::lower("main.star", &checked, variant, &requests)
        .expect("lowers")
        .program;
    let run = |response: &[u8]| {
        let mut vm = diag_ir::Vm::new(&program);
        let mut host = Answering {
            responses: [response.to_vec()].into(),
            logged: Vec::new(),
            sent: Vec::new(),
        };
        loop {
            match vm.step_with_declarations(&program, Some(&variant), &mut host) {
                Ok(diag_ir::StepOutcome::Continue) => {}
                Ok(diag_ir::StepOutcome::Finished) => return host.logged,
                other => panic!("{other:?}"),
            }
        }
    };
    assert_eq!(run(&[0x7E, 0x31]), ["negative", "out of range"]);
    // UDS's form is not negative under this shape.
    assert_eq!(run(&[0x7F, 0x22, 0x31]), ["positive", "no nrc", "7F"]);
}

// ---------------------------------------------------------------- `diag.read_dtc` (ADR-309, ADR-314)

const READ_DTCS: &str = "\
def main():
    res = diag.read_dtc(0x08)
    if res[\"ok\"]:
        diag.log(1, \"positive\")
    if res[\"nrc\"] == 0x31:
        diag.log(1, \"out of range\")
    if res[\"ok\"] and res[\"fields\"][\"mask\"] == 0xFF:
        diag.log(1, \"mask\")
    if res[\"raw\"] == b\"\\x59\\x02\\xff\\x12\\x34\\x56\\x09\":
        diag.log(1, \"one record\")
";

#[test]
fn a_dtc_read_sends_the_profile_s_request_to_the_declared_service() {
    // ReadDTCInformation by status mask, availability mask 0xFF, one DTC with its status.
    let host = run_answered(
        example(),
        READ_DTCS,
        &[&[0x59, 0x02, 0xFF, 0x12, 0x34, 0x56, 0x09]],
    )
    .unwrap();
    assert_eq!(host.logged, ["positive", "mask", "one record"]);
    // The UDS profile's request, SID first, to the variant's DTC read service.
    let converted = convert(with(example(), "main.star", READ_DTCS));
    let document = load_ecu_document(&converted.documents[0].bytes).expect("the document loads");
    let expected = document
        .variant()
        .and_then(|v| v.services())
        .and_then(|services| {
            services
                .iter()
                .find(|s| s.short_name() == Some("readDtcByStatusMask"))
        })
        .expect("the example declares it")
        .id();
    assert_eq!(host.sent, [(expected, vec![0x19, 0x02, 0x08])]);
    // A negative response reads through the declared shape.
    assert_eq!(
        answered(READ_DTCS, &[&[0x7F, 0x19, 0x31]]).unwrap(),
        ["out of range"]
    );
}

#[test]
fn a_dtc_read_is_checked_at_ingestion() {
    let codes = |source: &str| -> Vec<crate::star::StarCode> {
        procedures(with(example(), "main.star", source))
            .err()
            .map_or_else(Vec::new, |rejected| {
                rejected.errors.iter().map(|e| e.code).collect()
            })
    };
    use crate::star::StarCode::*;
    // The mask selects nothing, but it is sent in a fixed field: a constant in range.
    assert_eq!(
        codes("def f(m):\n    diag.read_dtc(m)\n\ndef main():\n    f(1)\n"),
        [NonConstantArgument]
    );
    assert_eq!(
        codes("def main():\n    diag.read_dtc(0x100)\n"),
        [ArgumentOutOfRange]
    );
    // The result has the profile's names, not the declared ones.
    assert_eq!(
        codes("def main():\n    x = diag.read_dtc(8)[\"fields\"][\"availabilityMask\"]\n"),
        [NotInProfile]
    );
    // `dtcs` is a list, which a procedure cannot read yet.
    assert_eq!(
        codes("def main():\n    x = diag.read_dtc(8)[\"fields\"][\"dtcs\"]\n"),
        [NotLowerable]
    );
    // A DTC result and a `diag.request` response of the same service are read differently.
    let mixed = "def main():\n    r = diag.read_dtc(8)\n    if r[\"ok\"]:\n        \
                 r = diag.request(0x19, {\"sub\": 2, \"statusMask\": 8})\n    \
                 x = r[\"fields\"][\"mask\"]\n";
    assert_eq!(codes(mixed), [NotLowerable]);
}

#[test]
fn a_dtc_read_selects_no_service_when_two_fit_the_profile() {
    let file = |name: &str| -> String {
        example()
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| String::from_utf8(c).expect("UTF-8"))
            .expect("a table")
    };
    // A second service with the same SID, constant and field widths, its fields named apart.
    let services = format!("{}readDtcAgain,0x19\n", file("services.csv"));
    let fields = format!(
        "{}request:readDtcAgain,sub,0,,uint,8,,,,,,0x02,,,\n\
         request:readDtcAgain,mask,1,,uint,8,,,,,,,,,\n\
         response:readDtcAgain,sub,0,,uint,8,,,,,,0x02,,,\n\
         response:readDtcAgain,mask,1,,uint,8,,,,,,,,,\n\
         response:readDtcAgain,records,2,,,,,,,,,,repeat,dtcRecord,\n",
        file("fields.csv")
    );
    let files = with(
        with(
            with(
                with(example(), "services.csv", &services),
                "fields.csv",
                &fields,
            ),
            "repeats.csv",
            &format!("{}response:readDtcAgain,records,end\n", file("repeats.csv")),
        ),
        "main.star",
        "def main():\n    diag.read_dtc(8)\n",
    );
    let errors = procedures(files).expect_err("rejected").errors;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].code, crate::star::StarCode::UnknownService);
    assert!(errors[0].hint.starts_with("2 services"), "{errors:?}");
}

#[test]
fn a_dtc_read_needs_one_service_that_fits_the_profile() {
    let fields = example()
        .into_iter()
        .find(|(n, _)| n == "fields.csv")
        .map(|(_, c)| String::from_utf8(c).expect("UTF-8"))
        .expect("fields.csv");
    let read = "def main():\n    diag.read_dtc(8)\n";
    let codes = |fields: String| -> Vec<(crate::star::StarCode, String)> {
        let files = with(with(example(), "fields.csv", &fields), "main.star", read);
        procedures(files).err().map_or_else(Vec::new, |rejected| {
            rejected
                .errors
                .into_iter()
                .map(|e| (e.code, e.hint))
                .collect()
        })
    };
    // A 16-bit mask field is not the profile's 8-bit one: no service matches.
    let wide = fields.replace(
        "request:readDtcByStatusMask,statusMask,1,,uint,8,",
        "request:readDtcByStatusMask,statusMask,1,,uint,16,",
    );
    assert_ne!(wide, fields);
    let found = codes(wide);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, crate::star::StarCode::UnknownService);
    // A 16-bit sub-function constant is not the profile's 8-bit one, even with the same
    // value, and the mask then starts a byte later.
    let wide_sub = fields
        .replace(
            "request:readDtcByStatusMask,sub,0,,uint,8,",
            "request:readDtcByStatusMask,sub,0,,uint,16,",
        )
        .replace(
            "request:readDtcByStatusMask,statusMask,1,",
            "request:readDtcByStatusMask,statusMask,2,",
        );
    let found = codes(wide_sub);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, crate::star::StarCode::UnknownService);
    // A 16-bit availability mask is not the width of a DTC's status.
    let wide_mask = fields
        .replace(
            "response:readDtcByStatusMask,availabilityMask,1,,uint,8,",
            "response:readDtcByStatusMask,availabilityMask,1,,uint,16,",
        )
        .replace(
            "response:readDtcByStatusMask,records,2,",
            "response:readDtcByStatusMask,records,3,",
        );
    let found = codes(wide_mask);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, crate::star::StarCode::NotInProfile);
    assert!(found[0].1.contains("availabilityMask"), "{found:?}");
    // A little-endian code would read the profile's code byte-reversed.
    let little: String = fields
        .lines()
        .enumerate()
        .map(|(at, line)| {
            let cell = match at {
                0 => "byte_order",
                _ if line.starts_with("layout:dtcRecord,dtc,") => "little",
                _ => "",
            };
            format!("{line},{cell}\n")
        })
        .collect();
    let found = codes(little);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, crate::star::StarCode::NotInProfile);
    assert!(found[0].1.contains("records"), "{found:?}");
    // Records whose code is 16 bits wide are not the profile's DTC records.
    let wide_status = fields.replace(
        "layout:dtcRecord,dtc,0,,uint,24,",
        "layout:dtcRecord,dtc,0,,uint,16,",
    );
    assert_ne!(wide_status, fields);
    let found = codes(wide_status);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, crate::star::StarCode::NotInProfile);
    assert!(found[0].1.contains("records"), "{found:?}");
    // A records field off a byte edge is rejected with the tables already (csv-tables 5.4).
    let unaligned = fields.replace(
        "response:readDtcByStatusMask,records,2,,",
        "response:readDtcByStatusMask,records,2,1,",
    );
    assert_ne!(unaligned, fields);
    let tables = crate::tables::read_files(with(example(), "fields.csv", &unaligned))
        .and_then(crate::tables::resolve);
    let errors = tables.expect_err("rejected by the tables").errors;
    assert!(
        errors.iter().any(|e| e.hint.contains("byte edge")),
        "{errors:?}"
    );
    // Records one byte after the mask leave a byte the profile's response does not have.
    let late = fields.replace(
        "response:readDtcByStatusMask,records,2,",
        "response:readDtcByStatusMask,records,3,",
    );
    assert_ne!(late, fields);
    let found = codes(late);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, crate::star::StarCode::NotInProfile);
    assert!(found[0].1.contains("`records` apart"), "{found:?}");
    // Records run to the end of the message, so nothing may follow them. The row is appended
    // to the table, whatever line ends the checkout gives it.
    let trailing = format!(
        "{}\nresponse:readDtcByStatusMask,trailer,,,uint,8,,,,,,,,,\n",
        fields.trim_end()
    );
    assert_ne!(trailing, fields);
    let found = codes(trailing);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, crate::star::StarCode::NotInProfile);
    assert!(found[0].1.contains("after `records`"), "{found:?}");
    // Records placed one after the other have the same layout, but padded ones do not.
    let sequential = fields
        .replace("layout:dtcRecord,dtc,0,,", "layout:dtcRecord,dtc,,,")
        .replace("layout:dtcRecord,status,3,,", "layout:dtcRecord,status,,,");
    assert_ne!(sequential, fields);
    assert_eq!(codes(sequential), []);
    let padded = with(
        with(
            example(),
            "layouts.csv",
            "name,byte_size\ndtcRecord,5\ndtcStatus,1\nwatchdogReset,0\n",
        ),
        "main.star",
        read,
    );
    let errors = procedures(padded).expect_err("rejected").errors;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].code, crate::star::StarCode::NotInProfile);
    // A BCD mask field would send the mask as decimal digits, and a BCD availability mask
    // would read its bits as digits: neither is the profile's raw mask.
    let bcd_request = fields.replace(
        "request:readDtcByStatusMask,statusMask,1,,uint,8,",
        "request:readDtcByStatusMask,statusMask,1,,bcd,8,",
    );
    let found = codes(bcd_request);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, crate::star::StarCode::UnknownService);
    let bcd_response = fields.replace(
        "response:readDtcByStatusMask,availabilityMask,1,,uint,8,",
        "response:readDtcByStatusMask,availabilityMask,1,,bcd,8,",
    );
    let found = codes(bcd_response);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, crate::star::StarCode::NotInProfile);
    // A response whose field at the mask's position is not an integer does not fit the map.
    let bytes_mask = fields.replace(
        "response:readDtcByStatusMask,availabilityMask,1,,uint,8,",
        "response:readDtcByStatusMask,availabilityMask,1,,bytes,8,",
    );
    assert_ne!(bytes_mask, fields);
    let found = codes(bytes_mask);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, crate::star::StarCode::NotInProfile);
    assert!(found[0].1.contains("availabilityMask"), "{found:?}");
}

#[test]
fn a_response_is_read_where_it_is_passed_and_each_call_sends_once() {
    let source = "\
def nrc_of(res):
    return res[\"nrc\"]

def read():
    return diag.request(0x22, {\"did\": 0xF190})

def main():
    first = read()
    second = read()
    if nrc_of(first) == 0x31 and second[\"ok\"]:
        diag.log(1, \"both\")
    diag.request(0x10, {\"sub\": 3})
";
    assert_eq!(
        answered(
            source,
            &[&[0x7F, 0x22, 0x31], &[0x62, 0xF1, 0x90], &[0x50, 0x03]]
        )
        .unwrap(),
        ["both"]
    );
    // The call used as a statement still sends: without a third answer the run fails.
    assert!(answered(source, &[&[0x7F, 0x22, 0x31], &[0x62]]).is_err());
}

#[test]
fn a_response_assigned_in_a_loop_is_read_after_it() {
    let source = "\
def main():
    res = diag.request(0x22, {\"did\": 0xF190})
    for i in range(3):
        if not res[\"ok\"]:
            res = diag.request(0x22, {\"did\": 0xF190})
    if res[\"ok\"]:
        diag.log(1, \"read\")
";
    let answers: [&[u8]; 2] = [&[0x7F, 0x22, 0x78], &[0x62, 0xF1, 0x90]];
    assert_eq!(answered(source, &answers).unwrap(), ["read"]);
}

#[test]
fn a_response_is_read_only_through_its_keys() {
    use crate::star::StarCode::{DynamicKeyNotAllowed, NotLowerable, UnsupportedSyntax};
    let read = |expr: &str| {
        format!("def main():\n    res = diag.request(0x22, {{\"did\": 0xF190}})\n    x = {expr}\n")
    };
    let cases = [
        (read("res[\"okay\"]"), UnsupportedSyntax),
        (read("res[\"fields\"] + 1"), NotLowerable),
        (read("res + b\"x\""), NotLowerable),
        (read("1 if res else 2"), NotLowerable),
        // Starlark would compare the dict: never equal to bytes, so a left-out `["raw"]`
        // would go unnoticed.
        (read("res == b\"\\x62\""), NotLowerable),
        (read("res != 0"), NotLowerable),
        (read("res == res"), NotLowerable),
        // A key must be a string literal also where the checker does not see a response.
        (
            "K = \"ok\"\n\ndef get():\n    return diag.request(0x22, {\"did\": 0xF190})\n\n\
             def main():\n    x = get()[K]\n"
                .to_owned(),
            DynamicKeyNotAllowed,
        ),
    ];
    for (source, code) in cases {
        let codes: Vec<_> = request_errors(&source).into_iter().map(|e| e.0).collect();
        assert_eq!(codes, [code], "{source}");
    }
}

// ---------------------------------------------------------------- Field reads (ADR-320)

/// The VIN response of the simulated ECU, and a flash state with phase 2 and block 7.
fn vin_answer() -> Vec<u8> {
    let mut answer = vec![0x62, 0xF1, 0x90];
    answer.extend_from_slice(b"NGRSIMECU00000001");
    answer
}

#[test]
fn a_field_read_decodes_the_declared_field() {
    let source = "\
def main():
    res = diag.request(0x22, {\"did\": 0xF190})
    if res[\"ok\"] and res[\"fields\"][\"vin\"] == \"NGRSIMECU00000001\":
        diag.log(1, \"vin\")
    state = diag.request(0x22, {\"did\": 0xFD00})[\"fields\"]
    if state[\"phase\"] == 2 and state[\"blockNumber\"] + 1 == 8:
        diag.log(1, \"state\")
    session = diag.request(0x22, {\"did\": 0xF186})
    if session[\"fields\"][\"session\"] == 3:
        diag.log(1, \"session\")
";
    let state: &[u8] = &[0x62, 0xFD, 0x00, 2, 0, 0, 0, 7, 0, 0, 1, 0];
    // The session's text table keeps the number.
    let session: &[u8] = &[0x62, 0xF1, 0x86, 3];
    assert_eq!(
        answered(source, &[&vin_answer(), state, session]).unwrap(),
        ["vin", "state", "session"]
    );
}

#[test]
fn a_float_field_reads_as_a_float() {
    // The supply voltage, declared here for every variant, is linear: 0x89 / 10 = 13.7.
    let files: Vec<_> = example()
        .into_iter()
        .map(|(name, bytes)| {
            let bytes = if name == "dids.csv" {
                String::from_utf8(bytes)
                    .unwrap()
                    .replace("exampleEcuV2,supplyVoltage", ",supplyVoltage")
                    .into_bytes()
            } else {
                bytes
            };
            (name, bytes)
        })
        .collect();
    let source = "\
def main():
    v = diag.request(0x22, {\"did\": 0xFD02})[\"fields\"][\"supplyVoltage\"]
    if v > 13.5 and v < 13.75:
        diag.log(1, \"in range\")
";
    assert_eq!(
        answered_in(files, source, &[&[0x62, 0xFD, 0x02, 0x00, 0x89]]).unwrap(),
        ["in range"]
    );
}

#[test]
fn a_field_of_a_negative_response_stops_the_run() {
    let source = "\
def main():
    vin = diag.request(0x22, {\"did\": 0xF190})[\"fields\"][\"vin\"]
";
    let error = answered(source, &[&[0x7F, 0x22, 0x31]]).unwrap_err();
    assert!(error.contains("Undecodable"), "{error}");
}

#[test]
fn a_field_read_is_checked_against_the_declarations() {
    use crate::star::StarCode::{DynamicKeyNotAllowed, NotLowerable, UnsupportedSyntax};
    let read = |expr: &str| {
        format!("def main():\n    res = diag.request(0x22, {{\"did\": 0xFD03}})\n    x = {expr}\n")
    };
    let cases = [
        // Not a field of this service's response.
        (read("res[\"fields\"][\"vin\"]"), UnsupportedSyntax),
        // A select: no scalar value.
        (read("res[\"fields\"][\"details\"]"), NotLowerable),
        (read("res[\"fields\"][\"reason\"] + \"x\""), NotLowerable),
        (read("res[\"fields\"] == res[\"fields\"]"), NotLowerable),
        // A key that is not a literal, also where the checker does not see a response.
        (
            "K = \"reason\"\n\ndef get():\n    return diag.request(0x22, {\"did\": 0xFD03})\n\n\
             def main():\n    x = get()[\"fields\"][K]\n"
                .to_owned(),
            DynamicKeyNotAllowed,
        ),
        // Two services on different paths: the field names are not known at ingestion.
        (
            "def main():\n    res = diag.request(0x22, {\"did\": 0xFD03})\n    \
             if res[\"ok\"]:\n        res = diag.request(0x22, {\"did\": 0xFD01})\n    \
             x = res[\"fields\"][\"reason\"]\n"
                .to_owned(),
            NotLowerable,
        ),
        // Types are per variable (4.8), so a variable reused for another service's response
        // cannot be read, even one assignment after the other.
        (
            "def main():\n    res = diag.request(0x22, {\"did\": 0xFD03})\n    \
             res = diag.request(0x22, {\"did\": 0xFD01})\n    \
             x = res[\"fields\"][\"reason\"]\n"
                .to_owned(),
            NotLowerable,
        ),
        // Nor a parameter whose callers pass responses of different services.
        (
            "def get(r):\n    return r[\"fields\"][\"reason\"]\n\n\
             def main():\n    x = get(diag.request(0x22, {\"did\": 0xFD03}))\n    \
             y = get(diag.request(0x22, {\"did\": 0xFD01}))\n"
                .to_owned(),
            NotLowerable,
        ),
    ];
    for (source, code) in cases {
        let codes: Vec<_> = request_errors(&source).into_iter().map(|e| e.0).collect();
        assert_eq!(codes, [code], "{source}");
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

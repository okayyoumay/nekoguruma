//! IR golden tests (design 12.1): the example ECU definition with `golden/procedure.star`,
//! converted to IR, the requests its programs send answered from `golden/responses.txt`, and the
//! values its declarations decode from that file, compared with the expected files in `golden/`.
//!
//! An intended change to any of them is recorded by running the test with
//! `NGR_UPDATE_GOLDEN=1`, which rewrites the expected files; review their diff before committing
//! it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use diag_frontend::convert::{lower_procedures, to_ir};
use diag_frontend::tables::{read_files, resolve};
use diag_ir::declaration::{EcuVariant, load_ecu_document};
use diag_ir::decode::decode_positive_response;
use diag_ir::{DiagHost, Program, StepOutcome, Vm};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A text file with its line endings made `\n`, so a Windows checkout reads the same bytes.
fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
}

/// The example definition's files with the golden procedure, by name.
fn inputs() -> Vec<(String, Vec<u8>)> {
    let dir = crate_dir().join("examples/example-ecu");
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .expect("example directory")
        .map(|entry| {
            let entry = entry.expect("entry");
            let name = entry.file_name().into_string().expect("UTF-8 name");
            (name, read_text(&entry.path()).into_bytes())
        })
        .collect();
    let procedure = read_text(&crate_dir().join("tests/golden/procedure.star"));
    files.push(("procedure.star".to_owned(), procedure.into_bytes()));
    files.sort();
    files
}

/// The inputs in `golden/`, which are not expected outputs.
const INPUTS: [&str; 2] = ["procedure.star", "responses.txt"];

/// Compares `actual` with the expected file `name`, or rewrites it under `NGR_UPDATE_GOLDEN`.
fn check(name: &str, actual: &str) {
    let path = crate_dir().join("tests/golden").join(name);
    if std::env::var_os("NGR_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        return;
    }
    let expected = read_text(&path);
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(e, a)| e != a)
            .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
        panic!(
            "{name} differs from the golden file from line {}:\n  expected: {:?}\n  actual:   {:?}\n\
             Run with NGR_UPDATE_GOLDEN=1 to accept an intended change.",
            line + 1,
            expected.lines().nth(line),
            actual.lines().nth(line),
        );
    }
}

/// A host that records each request and answers with the first response in `responses.txt`
/// that starts with the positive response SID and the request's first two bytes, or with just
/// those bytes, enough for the program to go on.
struct Recording {
    out: String,
    answers: Vec<Vec<u8>>,
}

#[derive(Debug)]
struct NotSent;

impl std::fmt::Display for NotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not sent by the golden procedure")
    }
}

impl std::error::Error for NotSent {}

fn hex(bytes: &[u8]) -> String {
    let parts: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
    parts.join(" ")
}

impl DiagHost for Recording {
    type Error = NotSent;

    fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, NotSent> {
        let sid = u8::try_from(service).expect("a one-byte SID");
        writeln!(self.out, "request {sid:02X} {}", hex(payload)).expect("a String");
        let mut response = vec![sid + 0x40];
        response.extend(payload.iter().take(2));
        Ok(self
            .answers
            .iter()
            .find(|answer| answer.starts_with(&response))
            .cloned()
            .unwrap_or(response))
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
    fn log(&mut self, level: u8, message: &str) {
        writeln!(self.out, "log {level} {message}").expect("a String");
    }
}

/// The requests and logs `program` produces with the declarations of `variant`, answered from
/// `answers`, then how it ends.
fn run(program: &Program, variant: &EcuVariant<'_>, answers: Vec<Vec<u8>>) -> String {
    let mut vm = Vm::new(program);
    let mut host = Recording {
        out: String::new(),
        answers,
    };
    for _ in 0..100_000 {
        match vm.step_with_declarations(program, Some(variant), &mut host) {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => {
                host.out.push_str("finished\n");
                return host.out;
            }
            other => panic!("the golden procedure stopped: {other:?}"),
        }
    }
    panic!("the golden procedure did not end")
}

#[test]
fn the_example_ecu_converts_runs_and_decodes_as_recorded() {
    let tables = read_files(inputs()).unwrap_or_else(|e| panic!("does not read: {e:?}"));
    let definition = resolve(tables).unwrap_or_else(|e| panic!("does not resolve: {e:?}"));
    let procedures =
        lower_procedures(&definition).unwrap_or_else(|e| panic!("does not lower: {e:?}"));
    let converted = to_ir(&definition, &procedures);

    let mut catalogs = String::new();
    for entry in &converted.texts {
        writeln!(catalogs, "text {} {:?}", entry.id, entry.text).expect("a String");
    }
    for entry in &converted.units {
        writeln!(catalogs, "unit {} {:?}", entry.id, entry.text).expect("a String");
    }
    let mut produced = vec!["catalogs.txt".to_owned()];
    check("catalogs.txt", &catalogs);

    let responses = read_text(&crate_dir().join("tests/golden/responses.txt"));
    for document in &converted.documents {
        let variant_name = &document.name;
        // One document per variant and procedure file, so both name the expected files.
        let procedure = document.procedure.as_deref().expect("a procedure");
        let name = format!(
            "{variant_name}.{}",
            procedure.strip_suffix(".star").unwrap_or(procedure)
        );
        let loaded = load_ecu_document(&document.bytes).expect("the document loads");
        let program: Program =
            postcard::from_bytes(loaded.procedure().expect("a procedure").bytes())
                .expect("a program");

        // The declaration part and the program, the procedure's bytes shown as the program.
        let mut unpacked = loaded.unpack();
        unpacked.procedure = None;
        produced.push(format!("{name}.ir.txt"));
        check(
            &format!("{name}.ir.txt"),
            &format!("{unpacked:#?}\n{program:#?}\n"),
        );

        let variant = loaded.variant().expect("a variant");
        let services = variant.services().expect("services");
        let mut answers = Vec::new();
        let mut decoded = String::new();
        for line in responses.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            let (Some(scope), Some(service)) = (words.next(), words.next()) else {
                panic!("responses.txt: {line}");
            };
            if scope != "*" && scope != variant_name {
                continue;
            }
            let bytes: Vec<u8> = words
                .map(|b| u8::from_str_radix(b, 16).unwrap_or_else(|_| panic!("hex: {line}")))
                .collect();
            let positive = services
                .iter()
                .find(|s| s.short_name() == Some(service))
                .and_then(|s| s.positive())
                .unwrap_or_else(|| panic!("{name} has no positive response for {service}"));
            let result = decode_positive_response(&positive, variant.layouts(), &bytes);
            writeln!(decoded, "{service} [{}]\n{result:#?}", hex(&bytes)).expect("a String");
            answers.push(bytes);
        }
        produced.push(format!("{name}.decoded.txt"));
        check(&format!("{name}.decoded.txt"), &decoded);

        produced.push(format!("{name}.run.txt"));
        check(
            &format!("{name}.run.txt"),
            &run(&program, &variant, answers),
        );
    }

    // Every expected file belongs to an output, so a variant or procedure that stops being
    // converted fails here instead of leaving its files unread.
    let dir = crate_dir().join("tests/golden");
    let mut stale: Vec<String> = std::fs::read_dir(&dir)
        .expect("the golden directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("UTF-8")
        })
        .filter(|file| !INPUTS.contains(&file.as_str()) && !produced.contains(file))
        .collect();
    stale.sort();
    if std::env::var_os("NGR_UPDATE_GOLDEN").is_some() {
        for file in &stale {
            std::fs::remove_file(dir.join(file)).expect("a stale golden file removed");
        }
    } else {
        assert!(
            stale.is_empty(),
            "golden files no output produces: {stale:?}; run with NGR_UPDATE_GOLDEN=1 to remove them"
        );
    }
}

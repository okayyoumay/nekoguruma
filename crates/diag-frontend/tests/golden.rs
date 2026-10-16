//! IR golden tests (design 12.1): the example ECU definition with `golden/procedure.star`,
//! converted to IR, the requests its programs send, and the values its declarations decode from
//! `golden/responses.txt`, compared with the expected files in `golden/`.
//!
//! An intended change to any of them is recorded by running the test with
//! `NGR_UPDATE_GOLDEN=1`, which rewrites the expected files; review their diff before committing
//! it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use diag_frontend::convert::{lower_procedures, to_ir};
use diag_frontend::tables::{read_files, resolve};
use diag_ir::declaration::load_ecu_document;
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

/// A host that records each request and answers with the positive response SID and the
/// request's first two bytes, enough for the program to go on.
#[derive(Default)]
struct Recording(String);

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
        writeln!(self.0, "request {sid:02X} {}", hex(payload)).expect("a String");
        let mut response = vec![sid + 0x40];
        response.extend(payload.iter().take(2));
        Ok(response)
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
        writeln!(self.0, "log {level} {message}").expect("a String");
    }
}

/// The requests and logs `program` produces, then how it ends.
fn run(program: &Program) -> String {
    let mut vm = Vm::new(program);
    let mut host = Recording::default();
    for _ in 0..100_000 {
        match vm.step(program, &mut host) {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => {
                host.0.push_str("finished\n");
                return host.0;
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
    check("catalogs.txt", &catalogs);

    let responses = read_text(&crate_dir().join("tests/golden/responses.txt"));
    for document in &converted.documents {
        let name = &document.name;
        let loaded = load_ecu_document(&document.bytes).expect("the document loads");
        let program: Program =
            postcard::from_bytes(loaded.procedure().expect("a procedure").bytes())
                .expect("a program");

        // The declaration part and the program, the procedure's bytes shown as the program.
        let mut unpacked = loaded.unpack();
        unpacked.procedure = None;
        check(
            &format!("{name}.ir.txt"),
            &format!("{unpacked:#?}\n{program:#?}\n"),
        );

        check(&format!("{name}.run.txt"), &run(&program));

        let variant = loaded.variant().expect("a variant");
        let services = variant.services().expect("services");
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
            if scope != "*" && scope != name {
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
        }
        check(&format!("{name}.decoded.txt"), &decoded);
    }
}

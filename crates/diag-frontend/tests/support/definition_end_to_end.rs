//! The end-to-end definition test's fixture (M2 exit criterion 1), shared by the test of each
//! worker kind through `#[path]`: `crates/j2534-0404-service/tests/definition_end_to_end.rs` and
//! `crates/iso22900-service/tests/ngr_agent_definition_end_to_end.rs`.
//!
//! The definition is the example ECU (`crates/diag-frontend/examples/example-ecu`), whose
//! variants are told apart by their software version (`variant_patterns.csv`), with one of the
//! procedures in `crates/diag-frontend/examples/end-to-end`. The simulated ECU both workers
//! answer from is configured by `end-to-end/ecu.json`.
//!
//! The variant is identified as a server would before it sends a job (design 8.9.1): each
//! variant's identification services are sent to the ECU, and `diag_ir::decode::select_variant`
//! picks the variant whose patterns the answers match (ADR-306, ADR-315).

use std::collections::HashMap;
use std::path::PathBuf;

use diag_frontend::convert::{lower_procedures, to_ir};
use diag_frontend::tables::{read_files, resolve};
use diag_ir::declaration::{encode_ecu_document, load_ecu_document};
use diag_ir::decode::{DecodedField, decode_positive_response, select_variant};
use diag_ir::encode::encode_request;
use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Value, VmState};

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../diag-frontend/examples")
}

/// The simulated ECU's configuration, for `NGR_SIM_ECU_CONFIG`.
pub fn ecu_config() -> PathBuf {
    examples().join("end-to-end/ecu.json")
}

/// [`ecu_config`] with `sw_version` instead of its software version, written to `path`.
#[allow(dead_code, reason = "only the D-PDU test identifies a second variant")]
pub fn ecu_config_with_version(path: &std::path::Path, sw_version: &str) -> PathBuf {
    let text = std::fs::read_to_string(ecu_config()).expect("the ECU configuration");
    let text = text.replace(
        "\"sw_version\": \"1.0.0\"",
        &format!("\"sw_version\": \"{sw_version}\""),
    );
    assert!(
        text.contains(sw_version),
        "the configuration names its software version"
    );
    std::fs::write(path, text).expect("the ECU configuration should be writable");
    path.to_owned()
}

/// One document per variant of the example ECU, in conversion order, each with `procedure` (a
/// file of `examples/end-to-end`) as its procedure: the variant's short name and the document.
pub fn documents(procedure: &str) -> Vec<(String, Vec<u8>)> {
    let read = |path: PathBuf| {
        // A Windows checkout may have CRLF line ends.
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .replace("\r\n", "\n")
            .into_bytes()
    };
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(examples().join("example-ecu"))
        .expect("example directory")
        .map(|entry| {
            let entry = entry.expect("entry");
            let name = entry.file_name().into_string().expect("UTF-8 name");
            (name, read(entry.path()))
        })
        .collect();
    files.push((
        procedure.to_owned(),
        read(examples().join("end-to-end").join(procedure)),
    ));
    files.sort();
    let tables = read_files(files).unwrap_or_else(|e| panic!("does not read: {e:?}"));
    let definition = resolve(tables).unwrap_or_else(|e| panic!("does not resolve: {e:?}"));
    let procedures =
        lower_procedures(&definition).unwrap_or_else(|e| panic!("does not lower: {e:?}"));
    to_ir(&definition, &procedures)
        .documents
        .into_iter()
        .map(|document| {
            let name = load_ecu_document(&document.bytes)
                .expect("the document loads")
                .variant()
                .expect("a loaded document has its variant")
                .short_name()
                .expect("a named variant")
                .to_owned();
            (name, document.bytes)
        })
        .collect()
}

/// The identification run of a variant's `document`: the document with its procedure replaced
/// by a program that sends the request of each service its variant patterns name, through the
/// generic send-receive, and leaves the answers on the stack. Returns the document and the
/// services in the order their answers are stacked.
pub fn identification(document: &[u8]) -> (Vec<u8>, Vec<u32>) {
    let loaded = load_ecu_document(document).expect("the document loads");
    let variant = loaded.variant().expect("a loaded document has its variant");
    let mut services: Vec<u32> = variant
        .patterns()
        .into_iter()
        .flatten()
        .map(|pattern| pattern.service_id())
        .collect();
    services.sort_unstable();
    services.dedup();
    let mut program = Program {
        schema_version: IR_SCHEMA_VERSION,
        code: Vec::new(),
        constants: Vec::new(),
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    };
    for &id in &services {
        let request = variant
            .services()
            .into_iter()
            .flatten()
            .find(|service| service.id() == id)
            .and_then(|service| service.request())
            .expect("an identification service has a request");
        let mut message = vec![request.sid()];
        message.extend(encode_request(&request, &HashMap::new()).expect("a constant request"));
        let constant = u32::try_from(program.constants.len()).expect("a few constants");
        program.constants.push(message);
        program.code.push(Op::PushBytes(constant));
        program.code.push(Op::SendReceive { service: id });
    }
    let mut unpacked = loaded.unpack();
    unpacked.procedure = Some(postcard::to_allocvec(&program).expect("the program serializes"));
    (encode_ecu_document(&unpacked), services)
}

/// The index in `documents` of the variant the identification runs select, or `None`. `runs`
/// gives, for each document in order, the services of its [`identification`] and the final
/// state of its run.
pub fn identified(documents: &[(String, Vec<u8>)], runs: &[(Vec<u32>, VmState)]) -> Option<usize> {
    let loaded: Vec<_> = documents
        .iter()
        .map(|(_, bytes)| load_ecu_document(bytes).expect("the document loads"))
        .collect();
    // Each answer decoded through its variant's own service; one that does not decode, such as
    // a negative response, matches no pattern.
    let mut decoded: HashMap<(usize, u32), Vec<DecodedField>> = HashMap::new();
    for (index, ((services, state), document)) in runs.iter().zip(&loaded).enumerate() {
        let variant = document
            .variant()
            .expect("a loaded document has its variant");
        assert_eq!(services.len(), state.stack.len(), "one answer per service");
        for (service, answer) in services.iter().zip(&state.stack) {
            let Value::Bytes(answer) = answer else {
                panic!("an answer is bytes: {answer:?}");
            };
            let positive = variant
                .services()
                .into_iter()
                .flatten()
                .find(|s| s.id() == *service)
                .and_then(|s| s.positive())
                .expect("an identification service has a positive response");
            if let Ok(fields) = decode_positive_response(&positive, variant.layouts(), answer) {
                decoded.insert((index, *service), fields);
            }
        }
    }
    select_variant(
        loaded.iter().map(|document| {
            document
                .variant()
                .expect("a loaded document has its variant")
        }),
        |index, service| decoded.get(&(index, service)).map(Vec::as_slice),
    )
}

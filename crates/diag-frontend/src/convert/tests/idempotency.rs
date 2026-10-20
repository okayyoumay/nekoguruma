//! The idempotency of each `diag` call's section (subset 4.6, ADR-332) against the example
//! ECU's declared services.

use super::*;

/// The program `main.star` lowers to for each variant of `files` (the example, or the example
/// with a service added).
fn programs_of(files: Vec<(String, Vec<u8>)>, source: &str) -> Vec<diag_ir::Program> {
    let converted = convert(with(files, "main.star", source));
    converted
        .documents
        .iter()
        .map(|converted| {
            let document = load_ecu_document(&converted.bytes).expect("the document loads");
            let bytes = document.procedure().expect("a program");
            postcard::from_bytes(bytes.bytes()).expect("a program")
        })
        .collect()
}

/// The idempotency of the one section over each diagnostic primitive of the program, in pc order,
/// the same for every variant.
fn idempotency_of(source: &str) -> Vec<String> {
    idempotency_in(example(), source)
}

fn idempotency_in(files: Vec<(String, Vec<u8>)>, source: &str) -> Vec<String> {
    let mut found: Vec<Vec<String>> = programs_of(files, source)
        .iter()
        .map(|program| {
            program.validate().expect("valid");
            (0..program.code.len() as u32)
                .filter(|&pc| program.code[pc as usize].is_diagnostic_primitive())
                .map(|pc| {
                    let covering: Vec<_> = program
                        .sections
                        .iter()
                        .filter(|s| (s.start_pc..s.end_pc).contains(&pc))
                        .collect();
                    let [section] = covering[..] else {
                        panic!("one section covers the primitive at {pc}");
                    };
                    assert_eq!((section.start_pc, section.end_pc), (pc, pc + 1));
                    format!("{:?}", section.idempotency)
                })
                .collect()
        })
        .collect();
    found.dedup();
    let [only] = &found[..] else {
        panic!("the variants differ: {found:?}");
    };
    only.clone()
}

#[test]
fn a_request_is_safe_when_it_reads_and_unsafe_otherwise() {
    let source = "def main():\n    \
                  diag.request(0x22, {\"did\": 0xF190})\n    \
                  diag.request(0x19, {\"statusMask\": 0xFF, \"sub\": 2})\n    \
                  diag.request(0x10, {\"sub\": 3})\n    \
                  diag.request(0x2E, {\"did\": 0xF190, \"vin\": \"WVWZZZ1JZXW000001\"})\n    \
                  # @idempotency safe\n    \
                  diag.request(0x2E, {\"did\": 0xF190, \"vin\": \"WVWZZZ1JZXW000001\"})\n    \
                  # @idempotency unsafe\n    \
                  diag.request(0x22, {\"did\": 0xF190})\n";
    assert_eq!(
        idempotency_of(source),
        ["Safe", "Safe", "Safe", "Unsafe", "Safe", "Unsafe"]
    );
}

#[test]
fn a_session_change_and_a_tester_present_default_to_safe_and_an_annotation_overrides() {
    // ADR-336 item 8: a repeat of either is harmless in any state.
    let source = "def main():\n    \
                  diag.request(0x10, {\"sub\": 3})\n    \
                  # @idempotency unsafe\n    \
                  diag.request(0x10, {\"sub\": 3})\n    \
                  diag.request(0x3E, {})\n    \
                  # @idempotency unsafe\n    \
                  diag.request(0x3E, {})\n";
    let files = super::services::appended(example(), "services.csv", &["testerPresent,0x3E"]);
    assert_eq!(
        idempotency_in(files, source),
        ["Safe", "Unsafe", "Safe", "Unsafe"]
    );
}

#[test]
fn a_routine_and_every_instruction_of_a_security_access_are_check_state() {
    let source =
        "def main():\n    diag.routine(0xFF01, 0x01, {})\n    diag.security_access(0x01)\n";
    let found = idempotency_of(source);
    // The routine, then the seed request, the key computation and the key request.
    assert!(found.len() >= 3, "{found:?}");
    assert!(found.iter().all(|value| value == "CheckState"), "{found:?}");
    // An annotation covers all of them.
    let annotated = "def main():\n    # @idempotency unsafe\n    diag.security_access(0x01)\n";
    let found = idempotency_of(annotated);
    assert!(found.len() >= 2, "{found:?}");
    assert!(found.iter().all(|value| value == "Unsafe"), "{found:?}");
}

#[test]
fn a_dtc_read_is_safe() {
    assert_eq!(
        idempotency_of("def main():\n    diag.read_dtc(0x09)\n"),
        ["Safe"]
    );
}

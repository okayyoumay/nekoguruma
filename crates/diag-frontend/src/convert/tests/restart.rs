//! The restart declaration of the tables in every program of a variant (ADR-336 items 1 and 6,
//! `csv-tables.md` 4.13, 4.14).

use diag_ir::{IdentityKind, Program, RuntimeInput, Source};

use super::*;

const NAMED: &str = "\
# @preconditions voltage externalSupply ignition
def main():
    diag.log(1, \"a\")
";
const PLAIN: &str = "def main():\n    diag.log(1, \"b\")\n";

/// The program of every document of `converted`, with the document's declarations.
fn programs(converted: &Converted) -> Vec<(EcuDocumentT, Program)> {
    documents(converted)
        .into_iter()
        .map(|document| {
            let bytes = document.procedure.clone().expect("a procedure");
            let program = postcard::from_bytes(&bytes).expect("a program");
            (document, program)
        })
        .collect()
}

/// The name of the service and of the field that `source` reads, in `document`.
fn source_names(document: &EcuDocumentT, source: Source) -> (String, String) {
    let Source::EcuService {
        service_id,
        field_id,
    } = source
    else {
        panic!("{source:?} is not a service source");
    };
    let service = variant(document)
        .services
        .iter()
        .flatten()
        .find(|s| s.id == service_id)
        .unwrap_or_else(|| panic!("no service {service_id}"));
    let field = service
        .positive
        .as_deref()
        .and_then(|p| p.fields.as_deref())
        .into_iter()
        .flatten()
        .find(|f| f.id == field_id)
        .unwrap_or_else(|| panic!("service {service_id} has no field {field_id}"));
    (
        service.short_name.clone().expect("a name"),
        field.short_name.clone().expect("a name"),
    )
}

#[test]
fn every_program_of_a_variant_carries_its_identity_and_the_named_preconditions() {
    let files = with(with(example(), "a.star", NAMED), "b.star", PLAIN);
    let converted = convert(files);
    let programs = programs(&converted);
    // Two variants, two procedures.
    assert_eq!(programs.len(), 4);
    for (index, (document, program)) in programs.iter().enumerate() {
        program.validate().expect("the declaration validates");
        let identity = &program.identity;
        let names = |kind: IdentityKind| {
            let source = match kind {
                IdentityKind::Vin => identity.vin,
                _ => unreachable!("the example declares only the VIN"),
            };
            source_names(document, source.expect("a source"))
        };
        assert_eq!(
            names(IdentityKind::Vin),
            ("vin.read".into(), "vin".into()),
            "{index}"
        );
        // The part number and version DIDs of the example are terminated text: no row.
        assert_eq!(identity.hardware_part_number, None);
        assert_eq!(identity.software_version, None);
        let named = index % 2 == 0;
        let p = &program.preconditions;
        assert_eq!(p.voltage_mv.is_some(), named, "{index}");
        assert_eq!(p.external_supply.is_some(), named);
        assert_eq!(p.ignition.is_some(), named);
        assert!(p.engine.is_none() && p.vehicle_speed.is_none());
        if named {
            let voltage = p.voltage_mv.as_ref().expect("voltage");
            assert_eq!(
                (voltage.satisfied.lower, voltage.satisfied.upper),
                (12_000, 15_000)
            );
            assert_eq!(
                voltage.default_session,
                Some(Source::RuntimeInput(RuntimeInput::SupplyVoltageMillivolts))
            );
            // Without a programming-session source, the default source serves there too.
            assert_eq!(voltage.programming_session, voltage.default_session);
        }
    }
}

#[test]
fn a_service_source_and_its_programming_source_take_the_declared_ids() {
    let files = with_restart(
        "kind,lower,upper,default_source,programming_source\n\
         voltage,1,9,powerCycles.read.powerCycles,bootTime.read.bootTime\n",
    );
    let converted = convert(with(
        files,
        "a.star",
        NAMED.replace(" externalSupply ignition", "").as_str(),
    ));
    for (document, program) in programs(&converted) {
        program.validate().expect("the declaration validates");
        let voltage = program.preconditions.voltage_mv.expect("voltage");
        let names = |source: Option<Source>| source_names(&document, source.expect("a source"));
        assert_eq!(
            names(voltage.default_session),
            ("powerCycles.read".into(), "powerCycles".into())
        );
        assert_eq!(
            names(voltage.programming_session),
            ("bootTime.read".into(), "bootTime".into())
        );
    }
}

/// The example with its `preconditions.csv` replaced by `contents` (with no `variants` column).
fn with_restart(contents: &str) -> Vec<(String, Vec<u8>)> {
    with(example(), "preconditions.csv", contents)
}

#[test]
fn a_name_without_a_row_in_a_variant_is_rejected_naming_the_variant() {
    // The second variant has all three rows the procedure names, the first only the voltage.
    let table = "variants,kind,lower,upper,default_source\n\
                 ,voltage,1,2,input:supply_voltage_mv\n\
                 exampleEcuV2,external_supply,1,1,input:external_supply\n\
                 exampleEcuV2,ignition,1,1,input:ignition_on\n";
    let definition =
        resolve(read_files(with(with_restart(table), "a.star", NAMED)).expect("reads"))
            .expect("resolves");
    let rejected = lower_procedures(&definition).expect_err("rejected");
    assert_eq!(rejected.errors.len(), 1);
    let error = &rejected.errors[0];
    assert_eq!(
        (error.code, error.line),
        (crate::star::StarCode::UnknownPrecondition, 1)
    );
    assert!(
        error.hint.contains("external_supply, ignition")
            && error.hint.ends_with("(in variant exampleEcuV1)"),
        "{}",
        error.hint
    );
    // Without the annotation the same definition lowers, declaring no precondition.
    let definition =
        resolve(read_files(with(with_restart(table), "a.star", PLAIN)).expect("reads"))
            .expect("resolves");
    let procedures = lower_procedures(&definition).expect("lowers");
    for bytes in &procedures[0].programs {
        let program: Program = postcard::from_bytes(bytes).expect("a program");
        assert_eq!(program.preconditions, diag_ir::Preconditions::default());
    }
}

#[test]
fn a_variant_without_the_tables_gets_a_program_without_the_facts() {
    let mut files = with(example(), "a.star", PLAIN);
    files.retain(|(n, _)| n != "identity.csv" && n != "preconditions.csv");
    let converted = convert(files);
    for (_, program) in programs(&converted) {
        assert_eq!(program.identity, diag_ir::IdentitySources::default());
        assert_eq!(program.preconditions, diag_ir::Preconditions::default());
        program.validate().expect("a plain program validates");
    }
}

// ---------------------------------------------------------------- `diag.precondition` (ADR-338)

/// A program that reads a precondition of each kind of source: the voltage from a runtime
/// input, the vehicle speed from the power-cycle count, a service source.
const READS: &str = "\
def main():
    if diag.precondition(\"voltage\") > 12000:
        diag.log(1, \"supply high\")
    if diag.precondition(\"vehicleSpeed\") == 7:
        diag.log(1, \"seven cycles\")
";

fn reads_table() -> Vec<(String, Vec<u8>)> {
    with_restart(
        "kind,lower,upper,default_source\n\
         voltage,12000,15000,input:supply_voltage_mv\n\
         vehicle_speed,0,9,powerCycles.read.powerCycles\n",
    )
}

#[test]
fn a_service_precondition_reads_the_did_and_a_runtime_one_asks_the_host() {
    let host = run_answered(reads_table(), READS, &[&[0x62, 0xFD, 0x01, 0, 0, 0, 7]])
        .expect("the program runs");
    assert_eq!(host.logged, ["supply high", "seven cycles"]);
    // The service source is a send-receive of the DID read; the input is read from the host.
    assert_eq!(host.sent.len(), 1);
    assert_eq!(host.sent[0].1, [0x22, 0xFD, 0x01]);
    assert_eq!(host.inputs_read, [RuntimeInput::SupplyVoltageMillivolts]);
    // The answer is decoded through the field: another count is another value.
    let host = run_answered(reads_table(), READS, &[&[0x62, 0xFD, 0x01, 0, 0, 0, 8]])
        .expect("the program runs");
    assert_eq!(host.logged, ["supply high"]);
}

#[test]
fn a_service_precondition_is_a_safe_send_receive_the_declarations_accept() {
    let converted = convert(with(reads_table(), "main.star", READS));
    for converted in &converted.documents {
        let document = load_ecu_document(&converted.bytes).expect("the document loads");
        let bytes = document.procedure().expect("a program");
        let program: Program = postcard::from_bytes(bytes.bytes()).expect("a program");
        program.validate().expect("valid");
        diag_ir::check_declarations(&program, &document.variant().expect("a variant"))
            .expect("the service and field are declared");
        let send = program
            .code
            .iter()
            .position(|op| matches!(op, diag_ir::Op::SendReceive { .. }))
            .expect("a send-receive");
        assert!(matches!(
            program.code[send + 1],
            diag_ir::Op::DecodeField { .. }
        ));
        let section = program
            .sections
            .iter()
            .find(|s| s.start_pc as usize <= send && send < s.end_pc as usize)
            .expect("a section over the primitive");
        assert!(
            matches!(section.idempotency, diag_ir::Idempotency::Safe),
            "{section:?}"
        );
        assert!(
            program
                .code
                .iter()
                .any(|op| matches!(op, diag_ir::Op::ReadInput { .. }))
        );
    }
}

#[test]
fn a_precondition_read_needs_a_row_of_its_kind_in_the_variant() {
    // The first variant has only the voltage row; the vehicle speed has none there.
    let table = "variants,kind,lower,upper,default_source\n\
                 ,voltage,1,2,input:supply_voltage_mv\n\
                 exampleEcuV2,vehicle_speed,0,9,powerCycles.read.powerCycles\n";
    let definition =
        resolve(read_files(with(with_restart(table), "a.star", READS)).expect("reads"))
            .expect("resolves");
    let rejected = lower_procedures(&definition).expect_err("rejected");
    assert_eq!(rejected.errors.len(), 1);
    assert_eq!(
        rejected.errors[0].code,
        crate::star::StarCode::UnknownPrecondition
    );
}

#[test]
fn a_service_precondition_with_a_converted_field_is_not_an_int() {
    // The supply voltage, declared here for every variant, has a linear conversion: its field
    // decodes to a float, so it cannot be the `int` the call gives.
    let files: Vec<_> = with(
        reads_table(),
        "preconditions.csv",
        "kind,lower,upper,default_source\n\
         voltage,1,9,supplyVoltage.read.supplyVoltage\n",
    )
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
    let source = "def main():\n    diag.precondition(\"voltage\")\n";
    let definition =
        resolve(read_files(with(files, "a.star", source)).expect("reads")).expect("resolves");
    let rejected = lower_procedures(&definition).expect_err("rejected");
    assert_eq!(rejected.errors.len(), 1);
    assert_eq!(rejected.errors[0].code, crate::star::StarCode::NotLowerable);
}

// ---------------------------------------------------------------- `diag.ecu_info` (ADR-339)

/// A VIN answer: the DID read's positive response, the identifier, then 17 ASCII bytes.
fn vin_answer() -> Vec<u8> {
    let mut answer = vec![0x62, 0xF1, 0x90];
    answer.extend_from_slice(b"WVWZZZ1JZXW000001");
    answer
}

/// The example's identity table with a part number and a software version as integer reads.
fn identity_table() -> Vec<(String, Vec<u8>)> {
    with(
        example(),
        "identity.csv",
        "variants,kind,service,field,note\n\
         ,vin,vin.read,vin,\n\
         ,hardware_part_number,powerCycles.read,powerCycles,\n\
         ,software_version,activeSession.read,session,\n",
    )
}

const ECU_INFO: &str = "\
def main():
    diag.log(1, diag.ecu_info()[\"variant\"])
    if diag.ecu_info()[\"vin\"] == \"WVWZZZ1JZXW000001\":
        diag.log(1, \"known vehicle\")
";

#[test]
fn ecu_info_reads_the_vin_and_names_the_variant() {
    let host = run_answered(example(), ECU_INFO, &[&vin_answer()]).expect("the program runs");
    assert_eq!(host.logged, ["exampleEcuV1", "known vehicle"]);
    // One DID read, with the request the declared VIN read sends.
    assert_eq!(host.sent.len(), 1);
    assert_eq!(host.sent[0].1, [0x22, 0xF1, 0x90]);
    assert!(host.inputs_read.is_empty());
    // Another VIN is another value: the answer is decoded through the field.
    let mut other = vin_answer();
    *other.last_mut().unwrap() = b'2';
    let host = run_answered(example(), ECU_INFO, &[&other]).expect("the program runs");
    assert_eq!(host.logged, ["exampleEcuV1"]);
}

#[test]
fn ecu_info_keys_are_separate_reads_in_the_order_of_access() {
    let source = "\
def main():
    if diag.ecu_info()[\"software_version\"] == 3:
        diag.log(1, \"extended\")
    if diag.ecu_info()[\"part_number\"] == 7:
        diag.log(1, \"seven\")
    if diag.ecu_info()[\"vin\"] == \"WVWZZZ1JZXW000001\":
        diag.log(1, \"vin\")
    if diag.ecu_info()[\"vin\"] == \"WVWZZZ1JZXW000001\":
        diag.log(1, \"vin again\")
";
    let host = run_answered(
        identity_table(),
        source,
        &[
            &[0x62, 0xF1, 0x86, 3],
            &[0x62, 0xFD, 0x01, 0, 0, 0, 7],
            &vin_answer(),
            &vin_answer(),
        ],
    )
    .expect("the program runs");
    assert_eq!(host.logged, ["extended", "seven", "vin", "vin again"]);
    let requests: Vec<_> = host.sent.iter().map(|(_, r)| r.clone()).collect();
    assert_eq!(
        requests,
        [
            vec![0x22, 0xF1, 0x86],
            vec![0x22, 0xFD, 0x01],
            vec![0x22, 0xF1, 0x90],
            vec![0x22, 0xF1, 0x90],
        ]
    );
}

#[test]
fn an_ecu_info_read_is_a_safe_send_receive_the_declarations_accept() {
    let converted = convert(with(example(), "main.star", ECU_INFO));
    for converted in &converted.documents {
        let document = load_ecu_document(&converted.bytes).expect("the document loads");
        let bytes = document.procedure().expect("a program");
        let program: Program = postcard::from_bytes(bytes.bytes()).expect("a program");
        program.validate().expect("valid");
        diag_ir::check_declarations(&program, &document.variant().expect("a variant"))
            .expect("the service and field are declared");
        let send = program
            .code
            .iter()
            .position(|op| matches!(op, diag_ir::Op::SendReceive { .. }))
            .expect("a send-receive");
        assert!(matches!(
            program.code[send + 1],
            diag_ir::Op::DecodeField { .. }
        ));
        let section = program
            .sections
            .iter()
            .find(|s| s.start_pc as usize <= send && send < s.end_pc as usize)
            .expect("a section over the primitive");
        assert!(
            matches!(section.idempotency, diag_ir::Idempotency::Safe),
            "{section:?}"
        );
    }
    // Each variant names itself.
    let names: Vec<_> = documents(&converted)
        .iter()
        .map(|d| variant(d).short_name.clone().expect("a name"))
        .collect();
    assert_eq!(names, ["exampleEcuV1", "exampleEcuV2"]);
}

#[test]
fn an_ecu_info_key_without_an_identity_row_is_unknown() {
    // The example declares only the VIN: its part number and version are terminated text.
    for key in ["part_number", "software_version"] {
        let source =
            format!("def main():\n    diag.log(1, \"x\")\n    diag.ecu_info()[\"{key}\"]\n");
        let definition = resolve(read_files(with(example(), "a.star", &source)).expect("reads"))
            .expect("resolves");
        let rejected = lower_procedures(&definition).expect_err("rejected");
        assert!(
            rejected
                .errors
                .iter()
                .all(|e| (e.code, e.line) == (crate::star::StarCode::UnknownIdentity, 3)),
            "{:?}",
            rejected.errors
        );
        assert!(!rejected.errors.is_empty());
    }
    // The variant name and the VIN need no more than the example has.
    convert(with(
        example(),
        "a.star",
        "def main():\n    diag.log(1, diag.ecu_info()[\"variant\"])\n",
    ));
}

#[test]
fn a_negative_ecu_info_answer_ends_the_run_with_a_vm_error() {
    let error = run_answered(example(), ECU_INFO, &[&[0x7F, 0x22, 0x31]])
        .err()
        .expect("the run stops");
    assert!(error.starts_with("Vm(Undecodable"), "{error}");
}

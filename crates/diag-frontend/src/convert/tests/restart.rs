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

//! `identity.csv`, `preconditions.csv` and the recovery columns of the flash tables (4.12 to
//! 4.14, ADR-336 item 1).

use diag_ir::{IdentityKind, PreconditionKind, RuntimeInput};

use super::*;

const IDENTITY: &str = "identity.csv";
const PRECONDITIONS: &str = "preconditions.csv";

#[test]
fn the_example_declares_its_identity_preconditions_and_recovery_facts() {
    let definition = resolve(read_files(example()).expect("reads")).expect("resolves");
    let tables = &definition.tables;
    let identity: Vec<_> = tables
        .identity
        .iter()
        .map(|r| (r.kind, r.service.name.as_str(), r.field.as_str()))
        .collect();
    assert_eq!(
        identity,
        [
            (IdentityKind::Vin, "vin", "vin"),
            (
                IdentityKind::HardwarePartNumber,
                "sparePartNumber",
                "sparePartNumber"
            ),
            (
                IdentityKind::SoftwareVersion,
                "softwareVersion",
                "softwareVersion"
            ),
        ]
    );
    let kinds: Vec<_> = tables.preconditions.iter().map(|r| r.kind).collect();
    assert_eq!(
        kinds,
        [
            PreconditionKind::Voltage,
            PreconditionKind::ExternalSupply,
            PreconditionKind::Ignition,
            PreconditionKind::Engine,
            PreconditionKind::VehicleSpeed,
        ]
    );
    let voltage = &tables.preconditions[0];
    assert_eq!((voltage.lower, voltage.upper), (12_000, 15_000));
    assert_eq!(
        voltage.default_source,
        SourceRef::Input(RuntimeInput::SupplyVoltageMillivolts)
    );
    assert_eq!(voltage.programming_source, None);
    // Both variants hold every row.
    for variant in &definition.variants {
        assert_eq!(variant.identity, [0, 1, 2]);
        assert_eq!(variant.preconditions, [0, 1, 2, 3, 4]);
    }
    let session = &tables.flash_sessions[0];
    assert_eq!(
        (
            session.session_timeout_ms,
            session.teardown_margin_ms,
            session.ecu_startup_ms,
            session.confirmation_window_ms,
            session.version_read_retries,
            session.no_application_nrc,
        ),
        (
            Some(5000),
            Some(1000),
            Some(2000),
            Some(3000),
            Some(3),
            Some(0x22)
        )
    );
    let segment = &tables.flash_segments[0];
    assert_eq!(
        (segment.data_format, segment.address_and_length_format),
        (0x00, 0x44)
    );
}

#[test]
fn the_format_bytes_of_a_segment_default_and_the_recovery_columns_are_optional() {
    let files = with(
        "flash_segments.csv",
        "session,name,start_address,length,artifact_offset,block_size\n\
         application,app,0,16,0,16\n",
    );
    let tables = read_files(files).expect("reads");
    let segment = &tables.flash_segments[0];
    assert_eq!(
        (segment.data_format, segment.address_and_length_format),
        (0x00, 0x44)
    );
    let files = with(
        "flash_sessions.csv",
        "id,name,steps,no_application_nrc\n1,application,ecu-reset,0x22\n",
    );
    let tables = read_files(files).expect("reads");
    let session = &tables.flash_sessions[0];
    assert_eq!(session.session_timeout_ms, None);
    assert_eq!(session.no_application_nrc, Some(0x22));
}

#[test]
fn recovery_columns_and_segment_formats_are_range_checked() {
    let header = "id,name,steps,session_timeout_ms,teardown_margin_ms,ecu_startup_ms,\
                  confirmation_window_ms,version_read_retries,no_application_nrc";
    assert_eq!(
        table_errors(
            "flash_sessions.csv",
            header,
            &[
                "1,a,ecu-reset,0,,,,,",
                "2,b,ecu-reset,4294967296,,,,,",
                "3,c,ecu-reset,1,-1,,,,",
                "4,d,ecu-reset,1,0,4294967296,,,",
                "5,e,ecu-reset,1,0,0,4294967296,,",
                "6,f,ecu-reset,1,0,0,0,256,",
                "7,g,ecu-reset,1,0,0,0,255,0x00",
                "8,h,ecu-reset,1,0,0,0,0,0x78",
                "9,i,ecu-reset,1,0,0,0,0,0x100",
                "10,j,ecu-reset,4294967295,4294967295,4294967295,4294967295,255,0xFF",
            ]
        ),
        vec![
            cell(BadValue, 2, "session_timeout_ms"),
            cell(BadValue, 3, "session_timeout_ms"),
            cell(BadValue, 4, "teardown_margin_ms"),
            cell(BadValue, 5, "ecu_startup_ms"),
            cell(BadValue, 6, "confirmation_window_ms"),
            cell(BadValue, 7, "version_read_retries"),
            cell(BadValue, 8, "no_application_nrc"),
            cell(BadValue, 9, "no_application_nrc"),
            cell(BadValue, 10, "no_application_nrc"),
        ]
    );
    assert_eq!(
        table_errors(
            "flash_segments.csv",
            "session,name,start_address,length,artifact_offset,block_size,data_format,\
             address_and_length_format",
            &[
                "application,a,0,1,0,1,0x100,",
                "application,b,1,1,1,1,,256",
                "application,c,2,1,2,1,0xFF,0xFF",
                // A nibble of 0 leaves the request without an address or a size.
                "application,d,3,1,3,1,0,0x40",
                "application,e,4,1,4,1,0,0x04",
                "application,f,5,1,5,1,0,0x00",
                "application,g,6,1,6,1,0,0x11",
            ]
        ),
        vec![
            cell(BadValue, 2, "data_format"),
            cell(BadValue, 3, "address_and_length_format"),
            cell(BadValue, 5, "address_and_length_format"),
            cell(BadValue, 6, "address_and_length_format"),
            cell(BadValue, 7, "address_and_length_format"),
        ]
    );
}

#[test]
fn identity_and_precondition_cells_are_checked() {
    assert_eq!(
        table_errors(
            IDENTITY,
            "kind,service,field",
            &[
                "serial,vin.read,vin",
                "vin,vin.erase,vin",
                "vin,vin.read,",
                ",vin.read,vin",
                "vin,vin.read,not-a-name",
            ]
        ),
        vec![
            cell(BadValue, 2, "kind"),
            cell(BadValue, 3, "service"),
            cell(MissingColumn, 4, "field"),
            cell(MissingColumn, 5, "kind"),
            cell(BadValue, 6, "field"),
        ]
    );
    let header = "kind,lower,upper,default_source,programming_source";
    assert_eq!(
        table_errors(
            PRECONDITIONS,
            header,
            &[
                // An unknown kind, a range with lower above upper, and a bound beyond i64.
                "battery,1,1,input:supply_voltage_mv,",
                "voltage,5,4,input:supply_voltage_mv,",
                "voltage,0,9223372036854775808,input:supply_voltage_mv,",
                "voltage,-9223372036854775809,0,input:supply_voltage_mv,",
                "voltage,-9223372036854775808,9223372036854775807,input:supply_voltage_mv,",
                // A flag is 0 or 1.
                "ignition,0,2,input:ignition_on,",
                "engine,-1,0,input:engine_running,",
                "external_supply,1,1,input:external_supply,",
                // Sources: no dot, a bad name, an unknown input.
                "voltage,1,2,supplyvoltage,",
                "voltage,1,2,vin.read.,",
                "voltage,1,2,input:battery,",
                "voltage,1,2,input:supply_voltage_mv,input:nothing",
            ]
        ),
        vec![
            cell(BadValue, 2, "kind"),
            cell(BadValue, 3, "upper"),
            cell(BadValue, 4, "upper"),
            cell(BadValue, 5, "lower"),
            cell(BadValue, 7, "upper"),
            cell(BadValue, 8, "lower"),
            cell(BadValue, 10, "default_source"),
            cell(BadValue, 11, "default_source"),
            cell(BadValue, 12, "default_source"),
            cell(BadValue, 13, "programming_source"),
        ]
    );
}

#[test]
fn an_input_must_report_the_fact_of_its_own_kind() {
    let header = "kind,lower,upper,default_source,programming_source";
    assert_eq!(
        table_errors(
            PRECONDITIONS,
            header,
            &[
                "voltage,1,2,input:ignition_on,",
                "voltage,1,2,input:supply_voltage_mv,input:vehicle_speed_kmh",
                "vehicle_speed,0,0,input:vehicle_speed_kmh,input:vehicle_speed_kmh",
                "engine,0,1,input:external_supply,input:engine_running",
            ]
        ),
        vec![
            cell(BadValue, 2, "default_source"),
            cell(BadValue, 3, "programming_source"),
            cell(BadValue, 5, "default_source"),
        ]
    );
}

/// The example with the DIDs of `dids` (`name,did` rows of `dids.csv`, readable) and the
/// `fields.csv` rows `fields` (in the example's column order, then `byte_order`) added, each padded
/// to the columns.
fn with_records(dids: &[&str], fields: &[&str]) -> Vec<(String, Vec<u8>)> {
    let mut dids_csv = example_text("dids.csv");
    for row in dids {
        dids_csv.push_str(&format!(",{row},yes,no,,,\n"));
    }
    let mut fields_csv = String::new();
    for (at, line) in example_text("fields.csv").lines().enumerate() {
        let extra = if at == 0 { "byte_order" } else { "" };
        fields_csv.push_str(&format!("{line},{extra}\n"));
    }
    // The rows give the cells up to `bits` (more if they need), `byte_order` last after `;`.
    for row in fields {
        let (cells, order) = row.split_once(';').unwrap_or((row, ""));
        let padding = ",".repeat(15 - cells.split(',').count());
        fields_csv.push_str(&format!("{cells}{padding},{order}\n"));
    }
    let mut files = with("dids.csv", &dids_csv);
    files.retain(|(n, _)| n != "fields.csv");
    files.push(("fields.csv".into(), fields_csv.into_bytes()));
    files
}

/// `files` with `identity` and `preconditions` rows replacing the example's tables.
fn with_restart(
    mut files: Vec<(String, Vec<u8>)>,
    identity: &[&str],
    preconditions: &[&str],
) -> Vec<(String, Vec<u8>)> {
    files.retain(|(n, _)| n != IDENTITY && n != PRECONDITIONS);
    let table = |header: &str, rows: &[&str]| {
        let mut text = format!("{header}\n");
        for row in rows {
            text.push_str(row);
            text.push('\n');
        }
        text.into_bytes()
    };
    files.push((
        IDENTITY.into(),
        table("variants,kind,service,field", identity),
    ));
    files.push((
        PRECONDITIONS.into(),
        table(
            "variants,kind,lower,upper,default_source,programming_source",
            preconditions,
        ),
    ));
    files
}

/// The errors of a definition whose tables are the example's, a few DIDs more, and the given
/// restart rows, as (code, file, line, column).
fn restart_errors(
    dids: &[&str],
    fields: &[&str],
    identity: &[&str],
    preconditions: &[&str],
) -> Vec<(ErrorCode, String, Option<u32>, Option<String>)> {
    resolved(with_restart(
        with_records(dids, fields),
        identity,
        preconditions,
    ))
}

const VIN: &str = ",vin,vin.read,vin";

#[test]
fn a_kind_is_given_once_per_variant() {
    // The same kind for different variants is two definitions of different documents.
    assert_eq!(
        restart_errors(
            &[],
            &[],
            &[
                "exampleEcuV1,vin,vin.read,vin",
                "exampleEcuV2,vin,vin.read,vin",
                "exampleEcuV1,vin,vin.read,vin",
                ",vin,vin.read,vin",
            ],
            &[
                "exampleEcuV1,voltage,1,2,input:supply_voltage_mv,",
                "exampleEcuV2,voltage,1,2,input:supply_voltage_mv,",
                ",voltage,1,2,input:supply_voltage_mv,",
            ],
        ),
        vec![
            at(Duplicate, IDENTITY, Some(4), Some("kind")),
            at(Duplicate, IDENTITY, Some(5), Some("kind")),
            at(Duplicate, PRECONDITIONS, Some(4), Some("kind")),
        ]
    );
}

#[test]
fn a_source_names_a_service_of_the_variant_that_is_a_generated_did_read() {
    assert_eq!(
        restart_errors(
            &[],
            &[],
            &[
                ",vin,nothing.read,vin",
                // A DID that only exists in the second variant, for every variant.
                ",hardware_part_number,supplyVoltage.read,supplyVoltage",
                // Not a read: the write of a DID, a declared service, a routine.
                ",software_version,vin.write,vin",
            ],
            &[
                ",voltage,1,2,defaultSession.sub,",
                ",ignition,1,1,input:ignition_on,eraseMemory.start.rid",
                ",engine,0,0,nothing.read.x,",
            ],
        ),
        vec![
            at(UnknownReference, IDENTITY, Some(2), Some("service")),
            at(UnknownReference, IDENTITY, Some(3), Some("service")),
            at(BadValue, IDENTITY, Some(4), Some("service")),
            at(BadValue, PRECONDITIONS, Some(2), Some("default_source")),
            at(BadValue, PRECONDITIONS, Some(3), Some("programming_source")),
            at(
                UnknownReference,
                PRECONDITIONS,
                Some(4),
                Some("default_source")
            ),
        ]
    );
    // A DID that only the scoped variant has is fine for a row of that variant.
    assert_eq!(
        restart_errors(
            &[],
            &[],
            &["exampleEcuV2,vin,vin.read,vin"],
            &["exampleEcuV2,voltage,1,2,supplyVoltage.read.supplyVoltage,"],
        ),
        vec![]
    );
}

#[test]
fn a_source_field_is_the_only_field_of_the_did() {
    let errors = |identity: &str, preconditions: &str| {
        restart_errors(&[], &[], &[identity], &[preconditions])
    };
    // A DID of several fields, and a field the response does not have.
    assert_eq!(
        errors(
            ",vin,flashState.read,phase",
            ",voltage,1,2,flashState.read.phase,"
        ),
        vec![
            at(BadValue, IDENTITY, Some(2), Some("field")),
            at(BadValue, PRECONDITIONS, Some(2), Some("default_source")),
        ]
    );
    assert_eq!(
        errors(
            ",vin,vin.read,serial",
            ",voltage,1,2,powerCycles.read.count,"
        ),
        vec![
            at(UnknownReference, IDENTITY, Some(2), Some("field")),
            at(
                UnknownReference,
                PRECONDITIONS,
                Some(2),
                Some("default_source")
            ),
        ]
    );
}

#[test]
fn a_source_field_has_the_type_the_agent_decodes() {
    let dids = [
        "floatValue,0xFD10",
        "signedValue,0xFD11",
        "bytesValue,0xFD12",
        "littleValue,0xFD13",
        "bitValue,0xFD14",
        "shortVin,0xFD16",
        "variableVin,0xFD17",
        "numberVin,0xFD18",
        "offsetValue,0xFD19",
        "wideValue,0xFD1A",
        "leadingText,0xFD1B",
    ];
    let fields = [
        "did:floatValue,floatValue,0,,float,32",
        "did:signedValue,signedValue,0,,int,16",
        "did:bytesValue,bytesValue,0,,bytes,16",
        "did:littleValue,littleValue,0,,uint,16;little",
        "did:bitValue,bitValue,0,,uint,12",
        "did:shortVin,shortVin,0,,ascii,128",
        "did:variableVin,variableVin,0,,ascii,,terminated,17,end",
        "did:numberVin,numberVin,0,,uint,64",
        "did:offsetValue,offsetValue,1,,uint,16",
        "did:wideValue,wideValue,0,,uint,56",
        "did:leadingText,leadingText,0,,ascii,8,leading",
    ];
    // An identity value is ASCII text or an unsigned integer, whatever the width.
    let kind_errors = |kind: &str, did: &str, field: &str| {
        restart_errors(
            &dids,
            &fields,
            &[&format!(",{kind},{did}.read,{field}")],
            &[],
        )
    };
    let bad = |errors: Vec<(ErrorCode, String, Option<u32>, Option<String>)>| {
        errors
            .into_iter()
            .map(|(code, file, _, column)| (code, file, column.unwrap_or_default()))
            .collect::<Vec<_>>()
    };
    let wrong = vec![(BadValue, IDENTITY.to_owned(), "field".to_owned())];
    for did in ["floatValue", "signedValue", "bytesValue"] {
        assert_eq!(
            bad(kind_errors("software_version", did, did)),
            wrong,
            "{did}"
        );
    }
    // The agent reads whole bytes as a big-endian integer from the record's first byte, into a
    // signed 64-bit value: seven bytes at most.
    for did in ["littleValue", "bitValue", "offsetValue", "numberVin"] {
        assert_eq!(
            bad(kind_errors("software_version", did, did)),
            wrong,
            "{did}"
        );
    }
    assert_eq!(
        bad(kind_errors("hardware_part_number", "shortVin", "shortVin")),
        vec![]
    );
    assert_eq!(
        bad(kind_errors("software_version", "wideValue", "wideValue")),
        vec![]
    );
    assert_eq!(
        bad(kind_errors("software_version", "shortVin", "shortVin")),
        vec![]
    );
    // An ASCII identity value has one fixed length: a terminated or length-prefixed text has
    // none the agent's source table can describe.
    for kind in ["hardware_part_number", "software_version"] {
        for did in ["variableVin", "leadingText"] {
            assert_eq!(bad(kind_errors(kind, did, did)), wrong, "{kind} {did}");
        }
    }
    // A VIN is a fixed 17-byte ASCII text: no other length, no terminated text, no number.
    assert_eq!(bad(kind_errors("vin", "vin", "vin")), vec![]);
    for did in ["shortVin", "variableVin", "numberVin"] {
        assert_eq!(bad(kind_errors("vin", did, did)), wrong, "{did}");
    }
    // A precondition's value is an unsigned integer of whole bytes, big-endian.
    let precondition = |did: &str| {
        bad(restart_errors(
            &dids,
            &fields,
            &[],
            &[&format!(",voltage,1,2,{did}.read.{did},")],
        ))
    };
    let wrong = vec![(
        BadValue,
        PRECONDITIONS.to_owned(),
        "default_source".to_owned(),
    )];
    for did in [
        "floatValue",
        "signedValue",
        "bytesValue",
        "littleValue",
        "bitValue",
        "shortVin",
        "variableVin",
        "offsetValue",
        "numberVin",
    ] {
        assert_eq!(precondition(did), wrong, "{did}");
    }
    assert_eq!(precondition("wideValue").len(), 0);
    assert_eq!(precondition("powerCycles").len(), 0);
    // The programming-session source is judged the same way.
    assert_eq!(
        restart_errors(
            &dids,
            &fields,
            &[],
            &[",voltage,1,2,powerCycles.read.powerCycles,shortVin.read.shortVin"],
        ),
        vec![at(
            BadValue,
            PRECONDITIONS,
            Some(2),
            Some("programming_source")
        )]
    );
    // And a service source resolves in the same stage as the rest: a VIN row is unaffected.
    assert_eq!(restart_errors(&[], &[], &[VIN], &[]), vec![]);
}

#[test]
fn a_definition_needs_neither_table() {
    let mut files = example();
    files.retain(|(n, _)| n != IDENTITY && n != PRECONDITIONS);
    let definition = resolve(read_files(files).expect("reads")).expect("resolves");
    assert!(definition.variants.iter().all(|v| v.identity.is_empty()));
}

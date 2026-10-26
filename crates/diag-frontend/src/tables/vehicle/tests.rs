use std::path::PathBuf;

use diag_ir::declaration::{EcuDocumentT, load_ecu_document};

use super::*;

fn example_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

/// The example vehicle definition's files, by name.
fn example() -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(example_dir("example-vehicle"))
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
fn with(file: &str, contents: &str) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<_> = example().into_iter().filter(|(n, _)| n != file).collect();
    files.push((file.to_owned(), contents.as_bytes().to_vec()));
    files
}

/// The example ECU's documents, as converted, read back.
fn example_ecu() -> Vec<EcuDocumentT> {
    let tables = crate::tables::read_directory(&example_dir("example-ecu")).expect("reads");
    let definition = crate::tables::resolve(tables).expect("resolves");
    let procedures = crate::convert::lower_procedures(&definition).expect("lowers");
    crate::convert::to_ir(&definition, &procedures)
        .documents
        .iter()
        .map(|document| load_ecu_document(&document.bytes).expect("loads").unpack())
        .collect()
}

type Located = (ErrorCode, String, Option<u32>, Option<String>);

fn located(rejected: Rejected) -> Vec<Located> {
    rejected
        .errors
        .into_iter()
        .map(|e| (e.code, e.file, e.line, e.column))
        .collect()
}

fn at(code: ErrorCode, file: &str, line: Option<u32>, column: Option<&str>) -> Located {
    (code, file.to_owned(), line, column.map(str::to_owned))
}

/// The errors of reading `files`.
fn read_errors(files: Vec<(String, Vec<u8>)>) -> Vec<Located> {
    read_vehicle_files(files)
        .err()
        .map_or_else(Vec::new, located)
}

/// The errors of converting `files` with the example ECU.
fn errors(files: Vec<(String, Vec<u8>)>) -> Vec<Located> {
    let tables = read_vehicle_files(files).expect("reads");
    let ecu = example_ecu();
    convert_vehicle(tables, &[&ecu])
        .err()
        .map_or_else(Vec::new, located)
}

#[test]
fn the_example_converts() {
    let tables = read_vehicle_directory(&example_dir("example-vehicle")).expect("reads");
    let ecu = example_ecu();
    let vehicle = convert_vehicle(tables, &[&ecu]).expect("converts");
    assert_eq!(vehicle.id, "exampleVehicle");
    assert_eq!(
        vehicle.identification,
        vec![
            (Component::Oem, vec!["Nekoguruma Motors".to_owned()]),
            (Component::VehicleModel, vec!["Example".to_owned()]),
            (
                Component::ModelYear,
                vec!["2026".to_owned(), "2027".to_owned()]
            ),
            (Component::VehicleType, vec!["Hatchback".to_owned()]),
        ]
    );
    let source = |variant: &str, service: &str, field: &str| Source {
        definition: "exampleEcu".to_owned(),
        variant: variant.to_owned(),
        service: service.to_owned(),
        field: field.to_owned(),
    };
    assert_eq!(
        vehicle.vin_rules,
        vec![VinRule {
            component: Component::Oem,
            value: "Nekoguruma Motors".to_owned(),
            start: 1,
            end: 3,
            expected: "NGR".to_owned(),
            source: source("exampleEcuV1", "vin.read", "vin"),
        }]
    );
    assert_eq!(
        vehicle.field_rules,
        vec![FieldRule {
            component: Component::ModelYear,
            value: "2027".to_owned(),
            source: source("exampleEcuV2", "softwareVersion.read", "softwareVersion"),
            expected: vec![0x32],
        }]
    );
    assert_eq!(
        vehicle.entries,
        vec![Entry {
            role: "engine".to_owned(),
            definitions: vec!["exampleEcu".to_owned()],
            optional: false,
            gateway: None,
        }]
    );
}

#[test]
fn the_file_set_and_cells_are_checked_first() {
    use ErrorCode::*;
    let mut missing = example();
    missing.retain(|(name, _)| name != "vehicle.csv");
    assert_eq!(
        read_errors(missing),
        vec![at(UnknownFile, "vehicle.csv", None, None)]
    );
    assert_eq!(
        read_errors(with("variants.csv", "id,name\n")),
        vec![at(UnknownFile, "variants.csv", None, None)]
    );
    assert_eq!(
        read_errors(with(
            "identification.csv",
            "component,value\nwheelbase,2.6\n"
        )),
        vec![at(
            BadValue,
            "identification.csv",
            Some(2),
            Some("component")
        )]
    );
    let vin = |row: &str| {
        with(
            "vin_rules.csv",
            &format!(
                "component,value,start,end,expected,definition,variant,service,field\n{row}\n"
            ),
        )
    };
    // The expected text spans the positions exactly.
    assert_eq!(
        read_errors(vin(
            "oem,Nekoguruma Motors,1,3,NG,exampleEcu,exampleEcuV1,vin.read,vin"
        )),
        vec![at(BadValue, "vin_rules.csv", Some(2), Some("expected"))]
    );
    assert_eq!(
        read_errors(vin(
            "oem,Nekoguruma Motors,3,1,NGR,exampleEcu,exampleEcuV1,vin.read,vin"
        )),
        vec![at(BadValue, "vin_rules.csv", Some(2), Some("end"))]
    );
    assert_eq!(
        read_errors(vin(
            "oem,Nekoguruma Motors,1,18,NGR,exampleEcu,exampleEcuV1,vin.read,vin"
        )),
        vec![at(BadValue, "vin_rules.csv", Some(2), Some("end"))]
    );
}

#[test]
fn the_rules_between_the_tables_are_checked() {
    use ErrorCode::*;
    let entries = |rows: &str| with("entries.csv", &format!("role,optional,gateway\n{rows}"));
    let alternatives = |rows: &str| {
        with(
            "entry_definitions.csv",
            &format!("role,order,definition\n{rows}"),
        )
    };
    let combined = |entries: &str, alternatives: &str| {
        let mut files: Vec<_> = example()
            .into_iter()
            .filter(|(n, _)| n != "entries.csv" && n != "entry_definitions.csv")
            .collect();
        files.push((
            "entries.csv".to_owned(),
            format!("role,optional,gateway\n{entries}").into_bytes(),
        ));
        files.push((
            "entry_definitions.csv".to_owned(),
            format!("role,order,definition\n{alternatives}").into_bytes(),
        ));
        files
    };
    // Duplicate role names.
    assert_eq!(
        errors(combined(
            "engine,no,\nengine,yes,\n",
            "engine,1,exampleEcu\n"
        )),
        vec![at(Duplicate, "entries.csv", Some(3), Some("role"))]
    );
    // A gateway that is not an entry of the definition.
    assert_eq!(
        errors(entries("engine,no,body\n")),
        vec![at(
            UnknownReference,
            "entries.csv",
            Some(2),
            Some("gateway")
        )]
    );
    // A cyclic gateway relation, reported on each entry in the cycle.
    assert_eq!(
        errors(combined(
            "engine,no,body\nbody,no,engine\ndoor,yes,engine\n",
            "engine,1,exampleEcu\nbody,1,exampleEcu\ndoor,1,exampleEcu\n"
        )),
        vec![
            at(BadValue, "entries.csv", Some(2), Some("gateway")),
            at(BadValue, "entries.csv", Some(3), Some("gateway")),
        ]
    );
    assert_eq!(
        errors(entries("engine,no,engine\n")),
        vec![at(BadValue, "entries.csv", Some(2), Some("gateway"))]
    );
    // An entry with no alternative, and an alternative of no entry.
    assert_eq!(
        errors(combined(
            "engine,no,\nbody,yes,\n",
            "engine,1,exampleEcu\ndoor,1,exampleEcu\n"
        )),
        vec![
            at(MissingColumn, "entries.csv", Some(3), Some("role")),
            at(
                UnknownReference,
                "entry_definitions.csv",
                Some(3),
                Some("role")
            ),
        ]
    );
    // One alternative per order, and each definition once per entry.
    assert_eq!(
        errors(alternatives("engine,1,exampleEcu\nengine,1,exampleEcu\n")),
        vec![
            at(
                Duplicate,
                "entry_definitions.csv",
                Some(3),
                Some("definition")
            ),
            at(Duplicate, "entry_definitions.csv", Some(3), Some("order")),
        ]
    );
    // A rule's value is one the definition states, and each value is stated once.
    assert_eq!(
        errors(with(
            "field_rules.csv",
            "component,value,definition,variant,service,field,expected\nmodel_year,2030,exampleEcu,exampleEcuV2,softwareVersion.read,softwareVersion,32\n"
        )),
        vec![at(
            UnknownReference,
            "field_rules.csv",
            Some(2),
            Some("value")
        )]
    );
    assert_eq!(
        errors(with(
            "identification.csv",
            "component,value\noem,Nekoguruma Motors\noem,Nekoguruma Motors\n"
        )),
        vec![
            // The model year the field rule names is no longer stated (errors sort by file).
            at(UnknownReference, "field_rules.csv", Some(2), Some("value")),
            at(Duplicate, "identification.csv", Some(3), Some("value")),
        ]
    );
    // One identifier.
    assert_eq!(
        errors(with("vehicle.csv", "id\nfirst\nsecond\n")),
        vec![at(Duplicate, "vehicle.csv", Some(3), None)]
    );
}

#[test]
fn the_ecu_definitions_named_are_checked() {
    use ErrorCode::*;
    // An identifier no ECU definition declares; the rules on the example ECU then read no ECU
    // of the vehicle's entries.
    assert_eq!(
        errors(with(
            "entry_definitions.csv",
            "role,order,definition\nengine,1,otherEcu\n"
        )),
        vec![
            at(
                UnknownReference,
                "entry_definitions.csv",
                Some(2),
                Some("definition")
            ),
            at(
                UnknownReference,
                "field_rules.csv",
                Some(2),
                Some("definition")
            ),
            at(
                UnknownReference,
                "vin_rules.csv",
                Some(2),
                Some("definition")
            ),
        ]
    );
    let field = |row: &str| {
        with(
            "field_rules.csv",
            &format!("component,value,definition,variant,service,field,expected\n{row}\n"),
        )
    };
    let refused = |row: &str| errors(field(row));
    assert_eq!(
        refused("model_year,2027,otherEcu,exampleEcuV2,softwareVersion.read,softwareVersion,32"),
        vec![at(
            UnknownReference,
            "field_rules.csv",
            Some(2),
            Some("definition")
        )]
    );
    assert_eq!(
        refused("model_year,2027,exampleEcu,exampleEcuV9,softwareVersion.read,softwareVersion,32"),
        vec![at(
            UnknownReference,
            "field_rules.csv",
            Some(2),
            Some("variant")
        )]
    );
    assert_eq!(
        refused("model_year,2027,exampleEcu,exampleEcuV2,boot.read,softwareVersion,32"),
        vec![at(
            UnknownReference,
            "field_rules.csv",
            Some(2),
            Some("service")
        )]
    );
    // A field that is not at the top of the read's positive response.
    assert_eq!(
        refused("model_year,2027,exampleEcu,exampleEcuV2,softwareVersion.read,vin,32"),
        vec![at(
            UnknownReference,
            "field_rules.csv",
            Some(2),
            Some("field")
        )]
    );
    // A service outside the read-only set (ADR-327 item 1).
    assert_eq!(
        refused("model_year,2027,exampleEcu,exampleEcuV2,ecuReset,reset,01"),
        vec![at(BadValue, "field_rules.csv", Some(2), Some("service"))]
    );
    // A VIN source whose field is not text.
    assert_eq!(
        errors(with(
            "vin_rules.csv",
            "component,value,start,end,expected,definition,variant,service,field\noem,Nekoguruma Motors,1,3,NGR,exampleEcu,exampleEcuV1,powerCycles.read,powerCycles\n"
        )),
        vec![at(BadValue, "vin_rules.csv", Some(2), Some("field"))]
    );
    // Two ECU definitions with one identifier cannot be named (ADR-363 item 3).
    let tables = read_vehicle_files(example()).expect("reads");
    let ecu = example_ecu();
    let twice = convert_vehicle(tables, &[&ecu, &ecu]).expect_err("refused");
    let twice = located(twice);
    assert!(
        twice.contains(&at(
            UnknownReference,
            "entry_definitions.csv",
            Some(2),
            Some("definition")
        )),
        "{twice:?}"
    );
}

#[test]
fn a_source_is_a_parameterless_scalar_read_of_one_of_the_vehicle_s_ecus() {
    use ErrorCode::*;
    let field = |row: &str| {
        with(
            "field_rules.csv",
            &format!("component,value,definition,variant,service,field,expected\n{row}\n"),
        )
    };
    // A read whose request needs a value (the status mask) has none to send.
    assert_eq!(
        errors(field(
            "model_year,2027,exampleEcu,exampleEcuV2,readDtcByStatusMask,availabilityMask,FF"
        )),
        vec![at(BadValue, "field_rules.csv", Some(2), Some("service"))]
    );
    // A shaped field (a selection) is no value to compare.
    assert_eq!(
        errors(field(
            "model_year,2027,exampleEcu,exampleEcuV2,lastResetReason.read,details,01"
        )),
        vec![at(BadValue, "field_rules.csv", Some(2), Some("field"))]
    );
    // The documents of one ECU definition all carry its identifier.
    let tables = read_vehicle_files(example()).expect("reads");
    let mut mixed = example_ecu();
    mixed.last_mut().expect("a document").definition_id = None;
    let refused = located(convert_vehicle(tables, &[&mixed]).expect_err("refused"));
    assert!(
        refused.contains(&at(
            UnknownReference,
            "entry_definitions.csv",
            Some(2),
            Some("definition")
        )),
        "{refused:?}"
    );
    // A rule reads one of the vehicle's own ECUs, one an entry names.
    let mut body = example_ecu();
    for document in &mut body {
        document.definition_id = Some("bodyEcu".to_owned());
    }
    let tables = read_vehicle_files(field(
        "model_year,2027,bodyEcu,exampleEcuV2,softwareVersion.read,softwareVersion,32",
    ))
    .expect("reads");
    let ecu = example_ecu();
    assert_eq!(
        located(convert_vehicle(tables, &[&ecu, &body]).expect_err("refused")),
        vec![at(
            UnknownReference,
            "field_rules.csv",
            Some(2),
            Some("definition")
        )]
    );
}

#[test]
fn reading_reports_what_the_ecu_tables_report() {
    use ErrorCode::*;
    // A VIN can hold no I, O or Q, nor small letters.
    assert_eq!(
        read_errors(with(
            "vin_rules.csv",
            "component,value,start,end,expected,definition,variant,service,field\noem,Nekoguruma Motors,1,3,NgR,exampleEcu,exampleEcuV1,vin.read,vin\n"
        )),
        vec![at(BadValue, "vin_rules.csv", Some(2), Some("expected"))]
    );
    // A file given twice, known or not.
    let mut twice = example();
    twice.push(("foo.csv".to_owned(), Vec::new()));
    twice.push(("foo.csv".to_owned(), Vec::new()));
    assert_eq!(
        read_errors(twice),
        vec![
            at(UnknownFile, "foo.csv", None, None),
            at(Duplicate, "foo.csv", None, None),
        ]
    );
    // A repeat names the first line it repeats.
    let tables = read_vehicle_files(with(
        "identification.csv",
        "component,value\nvehicle_model,Example\nvehicle_model,Example\nvehicle_model,Example\noem,Nekoguruma Motors\nmodel_year,2027\n",
    ))
    .expect("reads");
    let ecu = example_ecu();
    let rejected = convert_vehicle(tables, &[&ecu]).expect_err("refused");
    assert!(
        rejected
            .errors
            .iter()
            .filter(|e| e.code == Duplicate)
            .all(|e| e.hint.contains("line 2")),
        "{rejected}"
    );
    // A vehicle definition with no identifier or no entry.
    assert_eq!(
        errors(with("vehicle.csv", "id\n")),
        vec![at(MissingColumn, "vehicle.csv", None, None)]
    );
}

#[test]
fn the_names_are_checked_only_once_the_tables_hold() {
    use ErrorCode::*;
    let mut files: Vec<_> = example()
        .into_iter()
        .filter(|(n, _)| n != "entries.csv" && n != "entry_definitions.csv")
        .collect();
    files.push((
        "entries.csv".to_owned(),
        b"role,optional,gateway\nengine,no,\nengine,no,\n".to_vec(),
    ));
    files.push((
        "entry_definitions.csv".to_owned(),
        b"role,order,definition\nengine,1,nopeEcu\n".to_vec(),
    ));
    // The unknown identifier, a stage-3 error, is not reported beside the duplicate role.
    assert_eq!(
        errors(files),
        vec![at(Duplicate, "entries.csv", Some(3), Some("role"))]
    );
}

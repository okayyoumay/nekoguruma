//! Reading and converting a vehicle definition directory (`docs/vehicle-definition-tables.md`,
//! design 8.8.2, ADR-326, ADR-327, ADR-363).
//!
//! [`read_vehicle_directory`] (or [`read_vehicle_files`]) checks the file set, the dialect and
//! each table's cells, as an ECU definition's tables are checked. [`convert_vehicle`] then checks
//! the rules between the tables, and the names the definition gives the ECU definitions it is
//! converted with, and returns the [`VehicleDefinition`]. Each stage reports every error it finds
//! (ADR-298).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use diag_ir::declaration::{DataKind, EcuDocumentT, FieldT, ServiceT, ShapeT};

use super::ReadError;
use super::error::{ErrorCode, Errors, Rejected};
use super::records;
use super::rows::Line;
use super::sheet::{Sheet, Spec};
use super::values::{self, Parsed};

/// An identification component of a vehicle definition (ADR-326 item 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Component {
    Oem,
    VehicleModel,
    ModelYear,
    VehicleType,
}

impl Component {
    const WORDS: [(&'static str, Self); 4] = [
        ("oem", Self::Oem),
        ("vehicle_model", Self::VehicleModel),
        ("model_year", Self::ModelYear),
        ("vehicle_type", Self::VehicleType),
    ];

    /// The component as the tables write it.
    pub fn name(self) -> &'static str {
        Self::WORDS
            .iter()
            .find(|(_, component)| *component == self)
            .map(|(word, _)| *word)
            .expect("every component has a word")
    }
}

fn component(cell: &str) -> Parsed<Component> {
    values::keyword(cell, &Component::WORDS)
}

/// A `vehicle.csv` row: the definition's identifier.
#[derive(Debug, Clone, PartialEq)]
pub struct VehicleRow {
    pub line: Line,
    pub id: String,
}

/// An `identification.csv` row: one value of one component.
#[derive(Debug, Clone, PartialEq)]
pub struct IdentificationRow {
    pub line: Line,
    pub component: Component,
    pub value: String,
}

/// Where a rule reads its value: a service and a field of one variant of a named ECU definition
/// (ADR-327 items 4 and 5), as that definition's tables name them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Source {
    pub definition: String,
    pub variant: String,
    pub service: String,
    pub field: String,
}

/// A `vin_rules.csv` row.
#[derive(Debug, Clone, PartialEq)]
pub struct VinRuleRow {
    pub line: Line,
    pub component: Component,
    pub value: String,
    /// The first and last VIN position compared, from 1.
    pub start: u8,
    pub end: u8,
    pub expected: String,
    pub source: Source,
}

/// A `field_rules.csv` row.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldRuleRow {
    pub line: Line,
    pub component: Component,
    pub value: String,
    pub source: Source,
    /// Compared with the field's decoded value as variant patterns compare (ADR-306).
    pub expected: Vec<u8>,
}

/// An `entries.csv` row.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryRow {
    pub line: Line,
    pub role: String,
    pub optional: bool,
    pub gateway: Option<String>,
}

/// An `entry_definitions.csv` row: one alternative of an entry.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryDefinitionRow {
    pub line: Line,
    pub role: String,
    pub order: u16,
    pub definition: String,
}

/// The typed rows of a vehicle definition directory, in row order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VehicleTables {
    pub vehicle: Vec<VehicleRow>,
    pub identification: Vec<IdentificationRow>,
    pub vin_rules: Vec<VinRuleRow>,
    pub field_rules: Vec<FieldRuleRow>,
    pub entries: Vec<EntryRow>,
    pub entry_definitions: Vec<EntryDefinitionRow>,
}

const VEHICLE: Spec = Spec {
    file: "vehicle.csv",
    scoped: false,
    columns: &[("id", true)],
};

const IDENTIFICATION: Spec = Spec {
    file: "identification.csv",
    scoped: false,
    columns: &[("component", true), ("value", true)],
};

const SOURCE_COLUMNS: [&str; 4] = ["definition", "variant", "service", "field"];

const VIN_RULES: Spec = Spec {
    file: "vin_rules.csv",
    scoped: false,
    columns: &[
        ("component", true),
        ("value", true),
        ("start", true),
        ("end", true),
        ("expected", true),
        ("definition", true),
        ("variant", true),
        ("service", true),
        ("field", true),
    ],
};

const FIELD_RULES: Spec = Spec {
    file: "field_rules.csv",
    scoped: false,
    columns: &[
        ("component", true),
        ("value", true),
        ("definition", true),
        ("variant", true),
        ("service", true),
        ("field", true),
        ("expected", true),
    ],
};

const ENTRIES: Spec = Spec {
    file: "entries.csv",
    scoped: false,
    columns: &[("role", true), ("optional", false), ("gateway", false)],
};

const ENTRY_DEFINITIONS: Spec = Spec {
    file: "entry_definitions.csv",
    scoped: false,
    columns: &[("role", true), ("order", true), ("definition", true)],
};

const SPECS: [&Spec; 6] = [
    &VEHICLE,
    &IDENTIFICATION,
    &VIN_RULES,
    &FIELD_RULES,
    &ENTRIES,
    &ENTRY_DEFINITIONS,
];

/// The tables a vehicle definition must have.
const REQUIRED: [&str; 3] = ["vehicle.csv", "entries.csv", "entry_definitions.csv"];

/// The length of a VIN.
const VIN_LENGTH: u8 = 17;

/// Reads the tables of the vehicle definition directory `dir`. Only `.csv` entries directly in
/// it are read, following symbolic links; any other entry is skipped.
pub fn read_vehicle_directory(dir: &Path) -> Result<VehicleTables, ReadError> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.to_ascii_lowercase().ends_with(".csv") {
            continue;
        }
        let is_file = std::fs::metadata(entry.path()).is_ok_and(|m| m.is_file());
        let contents = if is_file && SPECS.iter().any(|spec| spec.file == name) {
            Some(std::fs::read(entry.path())?)
        } else if is_file {
            Some(Vec::new())
        } else {
            None
        };
        files.push((name, contents));
    }
    Ok(read(files)?)
}

/// Reads the tables from `files`, the names and contents of the files directly in a vehicle
/// definition directory.
pub fn read_vehicle_files(
    files: impl IntoIterator<Item = (String, Vec<u8>)>,
) -> Result<VehicleTables, Rejected> {
    read(
        files
            .into_iter()
            .map(|(name, contents)| (name, Some(contents))),
    )
}

/// Reads the tables; `None` contents mark an entry that is not a regular file.
fn read(
    files: impl IntoIterator<Item = (String, Option<Vec<u8>>)>,
) -> Result<VehicleTables, Rejected> {
    let mut errors = Errors::default();
    let mut sheets: HashMap<&'static str, Sheet> = HashMap::new();
    let mut seen = HashSet::new();
    for (name, contents) in files {
        if !name.to_ascii_lowercase().ends_with(".csv") {
            continue;
        }
        if !seen.insert(name.clone()) {
            errors.push(
                ErrorCode::Duplicate,
                &name,
                None,
                None,
                format!("{name} is given twice"),
            );
            continue;
        }
        let Some(spec) = SPECS.iter().find(|spec| spec.file == name) else {
            errors.push(
                ErrorCode::UnknownFile,
                &name,
                None,
                None,
                format!("{name} is not a table of a vehicle definition"),
            );
            continue;
        };
        let Some(contents) = contents else {
            errors.push(
                ErrorCode::UnknownFile,
                &name,
                None,
                None,
                format!("{name} is not a regular file"),
            );
            continue;
        };
        let sheet = match records::parse(&contents) {
            Ok(records) => Sheet::new(spec, records, &mut errors),
            Err(syntax) => {
                errors.push(
                    ErrorCode::Syntax,
                    spec.file,
                    Some(syntax.line),
                    None,
                    syntax.hint,
                );
                Sheet::new(spec, Vec::new(), &mut errors)
            }
        };
        sheets.insert(spec.file, sheet);
    }
    for file in REQUIRED {
        if !sheets.contains_key(file) {
            errors.push(
                ErrorCode::UnknownFile,
                file,
                None,
                None,
                format!("the required table {file} is missing"),
            );
        }
    }
    for spec in SPECS {
        sheets
            .entry(spec.file)
            .or_insert_with(|| Sheet::new(spec, Vec::new(), &mut Errors::default()));
    }
    let e = &mut errors;
    let tables = VehicleTables {
        vehicle: sheets[VEHICLE.file].rows(e, |row, e| {
            let id = row.required(e, "id", values::name);
            Some(VehicleRow {
                line: row.line,
                id: id?,
            })
        }),
        identification: sheets[IDENTIFICATION.file].rows(e, |row, e| {
            let component = row.required(e, "component", component);
            let value = row.required(e, "value", values::text);
            Some(IdentificationRow {
                line: row.line,
                component: component?,
                value: value?,
            })
        }),
        vin_rules: sheets[VIN_RULES.file].rows(e, |row, e| {
            let component = row.required(e, "component", component);
            let value = row.required(e, "value", values::text);
            let position = |cell: &str| {
                values::integer(cell, false, 1, VIN_LENGTH.into()).map(|value| value as u8)
            };
            let start = row.required(e, "start", position);
            let end = row.required(e, "end", position);
            let expected = row.required(e, "expected", values::text);
            let source = source(row, e);
            let (start, end, expected) = (start?, end?, expected?);
            if end < start {
                row.error(e, ErrorCode::BadValue, "end", "end is before start");
                return None;
            }
            // A well-formed VIN has digits and capital letters other than I, O and Q
            // (ADR-261 item 3), so any other text could never match.
            let vin_character =
                |c: char| c.is_ascii_digit() || (c.is_ascii_uppercase() && !"IOQ".contains(c));
            if !expected.chars().all(vin_character)
                || expected.len() != usize::from(end - start + 1)
            {
                row.error(
                    e,
                    ErrorCode::BadValue,
                    "expected",
                    format!(
                        "expected must be {} VIN characters (0-9, A-Z but I, O and Q), one per position from start to end",
                        end - start + 1
                    ),
                );
                return None;
            }
            Some(VinRuleRow {
                line: row.line,
                component: component?,
                value: value?,
                start,
                end,
                expected,
                source: source?,
            })
        }),
        field_rules: sheets[FIELD_RULES.file].rows(e, |row, e| {
            let component = row.required(e, "component", component);
            let value = row.required(e, "value", values::text);
            let source = source(row, e);
            let expected = row.required(e, "expected", values::bytes);
            Some(FieldRuleRow {
                line: row.line,
                component: component?,
                value: value?,
                source: source?,
                expected: expected?,
            })
        }),
        entries: sheets[ENTRIES.file].rows(e, |row, e| {
            let role = row.required(e, "role", values::name);
            let optional = row.or(e, "optional", false, values::boolean);
            let gateway = row.optional(e, "gateway", values::name);
            Some(EntryRow {
                line: row.line,
                role: role?,
                optional: optional?,
                gateway: gateway.ok()?,
            })
        }),
        entry_definitions: sheets[ENTRY_DEFINITIONS.file].rows(e, |row, e| {
            let role = row.required(e, "role", values::name);
            let order = row.required(e, "order", |cell| {
                values::integer(cell, false, 1, u16::MAX.into()).map(|value| value as u16)
            });
            let definition = row.required(e, "definition", values::name);
            Some(EntryDefinitionRow {
                line: row.line,
                role: role?,
                order: order?,
                definition: definition?,
            })
        }),
    };
    if errors.0.is_empty() {
        Ok(tables)
    } else {
        Err(Rejected::new(errors.0))
    }
}

/// A rule's source, from its four columns; every cell's error is reported.
fn source(row: &mut super::sheet::Row<'_>, errors: &mut Errors) -> Option<Source> {
    let [definition, variant, service, field] = SOURCE_COLUMNS.map(|column| {
        if column == "service" {
            row.required(errors, column, |cell| {
                values::service_name(cell).map(|_| cell.to_owned())
            })
        } else {
            row.required(errors, column, values::name)
        }
    });
    Some(Source {
        definition: definition?,
        variant: variant?,
        service: service?,
        field: field?,
    })
}

/// A converted vehicle definition (design 8.8.2, ADR-326).
#[derive(Debug, Clone, PartialEq)]
pub struct VehicleDefinition {
    pub id: String,
    /// Each component's values, in row order, for the components that have any.
    pub identification: Vec<(Component, Vec<String>)>,
    pub vin_rules: Vec<VinRule>,
    pub field_rules: Vec<FieldRule>,
    pub entries: Vec<Entry>,
}

/// A VIN matching rule: the VIN's positions `start` to `end` (from 1) equal `expected` when the
/// vehicle's `component` is `value`; the VIN is read from `source` (ADR-327 item 5).
#[derive(Debug, Clone, PartialEq)]
pub struct VinRule {
    pub component: Component,
    pub value: String,
    pub start: u8,
    pub end: u8,
    pub expected: String,
    pub source: Source,
}

/// A read-field matching rule: the field of `source` matches `expected` when the vehicle's
/// `component` is `value`, compared as variant patterns compare (ADR-306, ADR-327 item 4).
#[derive(Debug, Clone, PartialEq)]
pub struct FieldRule {
    pub component: Component,
    pub value: String,
    pub source: Source,
    pub expected: Vec<u8>,
}

/// An ECU entry of the topology (ADR-326 item 3).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub role: String,
    /// The ECU definitions that may fill it, in order.
    pub definitions: Vec<String>,
    pub optional: bool,
    /// The role of the entry it is reached through.
    pub gateway: Option<String>,
}

/// Converts `tables`, naming the ECU definitions in `ecu_definitions`: each the documents one
/// ECU definition converted to, which carry its identifier (ADR-363). The rules between the
/// tables are checked first, then the names given to the ECU definitions.
pub fn convert_vehicle(
    tables: VehicleTables,
    ecu_definitions: &[&[EcuDocumentT]],
) -> Result<VehicleDefinition, Rejected> {
    let mut errors = Errors::default();
    check_tables(&tables, &mut errors);
    if errors.0.is_empty() {
        check_names(&tables, ecu_definitions, &mut errors);
    }
    if !errors.0.is_empty() {
        return Err(Rejected::new(errors.0));
    }
    let mut identification: Vec<(Component, Vec<String>)> = Vec::new();
    for row in &tables.identification {
        match identification.iter_mut().find(|(c, _)| *c == row.component) {
            Some((_, values)) => values.push(row.value.clone()),
            None => identification.push((row.component, vec![row.value.clone()])),
        }
    }
    let entries = tables
        .entries
        .iter()
        .map(|entry| {
            let mut alternatives: Vec<&EntryDefinitionRow> = tables
                .entry_definitions
                .iter()
                .filter(|row| row.role == entry.role)
                .collect();
            alternatives.sort_by_key(|row| row.order);
            Entry {
                role: entry.role.clone(),
                definitions: alternatives
                    .into_iter()
                    .map(|row| row.definition.clone())
                    .collect(),
                optional: entry.optional,
                gateway: entry.gateway.clone(),
            }
        })
        .collect();
    Ok(VehicleDefinition {
        id: tables.vehicle[0].id.clone(),
        identification,
        vin_rules: tables
            .vin_rules
            .iter()
            .map(|row| VinRule {
                component: row.component,
                value: row.value.clone(),
                start: row.start,
                end: row.end,
                expected: row.expected.clone(),
                source: row.source.clone(),
            })
            .collect(),
        field_rules: tables
            .field_rules
            .iter()
            .map(|row| FieldRule {
                component: row.component,
                value: row.value.clone(),
                source: row.source.clone(),
                expected: row.expected.clone(),
            })
            .collect(),
        entries,
    })
}

/// The line `key` was first given on, or `None` (recording `line`) when this is the first.
fn first_line<K: Eq + std::hash::Hash>(
    map: &mut HashMap<K, Line>,
    key: K,
    line: Line,
) -> Option<Line> {
    match map.entry(key) {
        std::collections::hash_map::Entry::Occupied(first) => Some(*first.get()),
        std::collections::hash_map::Entry::Vacant(vacant) => {
            vacant.insert(line);
            None
        }
    }
}

/// The rules between the tables of the vehicle definition itself.
fn check_tables(tables: &VehicleTables, errors: &mut Errors) {
    // One identifier.
    match tables.vehicle.as_slice() {
        [] => errors.push(
            ErrorCode::MissingColumn,
            VEHICLE.file,
            None,
            None,
            "a vehicle definition needs its identifier",
        ),
        [first, rest @ ..] => {
            for row in rest {
                errors.push(
                    ErrorCode::Duplicate,
                    VEHICLE.file,
                    Some(row.line),
                    None,
                    format!(
                        "a vehicle definition has one identifier, already given on line {}",
                        first.line
                    ),
                );
            }
        }
    }
    // Each value of a component once.
    let mut values: HashMap<(Component, &str), Line> = HashMap::new();
    for row in &tables.identification {
        if let Some(earlier) = first_line(&mut values, (row.component, &row.value), row.line) {
            errors.push(
                ErrorCode::Duplicate,
                IDENTIFICATION.file,
                Some(row.line),
                Some("value"),
                format!(
                    "{} {:?} is already given on line {earlier}",
                    row.component.name(),
                    row.value
                ),
            );
        }
    }
    // A rule establishes a value the definition states.
    let rules = tables
        .vin_rules
        .iter()
        .map(|row| (VIN_RULES.file, row.line, row.component, &row.value))
        .chain(
            tables
                .field_rules
                .iter()
                .map(|row| (FIELD_RULES.file, row.line, row.component, &row.value)),
        );
    for (file, line, component, value) in rules {
        if !values.contains_key(&(component, value.as_str())) {
            errors.push(
                ErrorCode::UnknownReference,
                file,
                Some(line),
                Some("value"),
                format!(
                    "{} {value:?} is not a value identification.csv gives",
                    component.name()
                ),
            );
        }
    }
    // Role names are unique.
    let mut roles: HashMap<&str, Line> = HashMap::new();
    for row in &tables.entries {
        if let Some(earlier) = first_line(&mut roles, &row.role, row.line) {
            errors.push(
                ErrorCode::Duplicate,
                ENTRIES.file,
                Some(row.line),
                Some("role"),
                format!("role {} is already defined on line {earlier}", row.role),
            );
        }
    }
    if tables.entries.is_empty() {
        errors.push(
            ErrorCode::MissingColumn,
            ENTRIES.file,
            None,
            None,
            "a vehicle definition needs at least one ECU entry",
        );
    }
    // A gateway is an entry of the same definition, and the relation has no cycle.
    for row in &tables.entries {
        if let Some(gateway) = &row.gateway
            && !roles.contains_key(gateway.as_str())
        {
            errors.push(
                ErrorCode::UnknownReference,
                ENTRIES.file,
                Some(row.line),
                Some("gateway"),
                format!("gateway {gateway} is not an entry of this vehicle definition"),
            );
        }
    }
    let gateway_of: HashMap<&str, &str> = tables
        .entries
        .iter()
        .filter_map(|row| Some((row.role.as_str(), row.gateway.as_deref()?)))
        .collect();
    for row in &tables.entries {
        let mut seen = HashSet::from([row.role.as_str()]);
        let mut at = row.role.as_str();
        while let Some(&next) = gateway_of.get(at) {
            if !seen.insert(next) {
                // Reported on each entry whose own chain comes back to it: every entry in a
                // cycle, not one that only leads into it.
                if next == row.role {
                    errors.push(
                        ErrorCode::BadValue,
                        ENTRIES.file,
                        Some(row.line),
                        Some("gateway"),
                        format!("entry {} is reached through itself", row.role),
                    );
                }
                break;
            }
            at = next;
        }
    }
    // Each alternative names an entry, once, at an order of its own; every entry has one.
    let mut orders: HashMap<(&str, u16), Line> = HashMap::new();
    let mut alternatives: HashMap<(&str, &str), Line> = HashMap::new();
    for row in &tables.entry_definitions {
        if !roles.contains_key(row.role.as_str()) {
            errors.push(
                ErrorCode::UnknownReference,
                ENTRY_DEFINITIONS.file,
                Some(row.line),
                Some("role"),
                format!(
                    "role {} is not an entry of this vehicle definition",
                    row.role
                ),
            );
        }
        if let Some(earlier) = first_line(&mut orders, (&row.role, row.order), row.line) {
            errors.push(
                ErrorCode::Duplicate,
                ENTRY_DEFINITIONS.file,
                Some(row.line),
                Some("order"),
                format!(
                    "role {} already has an alternative at order {} on line {earlier}",
                    row.role, row.order
                ),
            );
        }
        if let Some(earlier) = first_line(&mut alternatives, (&row.role, &row.definition), row.line)
        {
            errors.push(
                ErrorCode::Duplicate,
                ENTRY_DEFINITIONS.file,
                Some(row.line),
                Some("definition"),
                format!(
                    "role {} already names {} on line {earlier}",
                    row.role, row.definition
                ),
            );
        }
    }
    for row in &tables.entries {
        if !tables
            .entry_definitions
            .iter()
            .any(|alternative| alternative.role == row.role)
        {
            errors.push(
                ErrorCode::MissingColumn,
                ENTRIES.file,
                Some(row.line),
                Some("role"),
                format!(
                    "entry {} has no ECU definition in entry_definitions.csv",
                    row.role
                ),
            );
        }
    }
}

/// The names the definition gives the ECU definitions it is converted with: each identifier
/// names exactly one of them, and each rule's source is a field at the top of the positive
/// response of a read-only service of that definition's named variant (ADR-327 item 1).
fn check_names(tables: &VehicleTables, ecu_definitions: &[&[EcuDocumentT]], errors: &mut Errors) {
    // The ECU definitions by identifier; one declared twice cannot be named.
    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, documents) in ecu_definitions.iter().enumerate() {
        let ids: HashSet<&str> = documents
            .iter()
            .filter_map(|document| document.definition_id.as_deref())
            .collect();
        for id in ids {
            by_id.entry(id).or_default().push(index);
        }
    }
    let named = |id: &str| -> Result<&[EcuDocumentT], String> {
        let documents = match by_id.get(id).map(Vec::as_slice) {
            None | Some([]) => {
                return Err(format!(
                    "no ECU definition this vehicle definition is converted with has the identifier {id}"
                ));
            }
            Some([one]) => ecu_definitions[*one],
            Some(_) => {
                return Err(format!(
                    "more than one ECU definition has the identifier {id}"
                ));
            }
        };
        // Every document of one definition carries its identifier (ADR-363 item 2).
        if documents
            .iter()
            .all(|document| document.definition_id.as_deref() == Some(id))
        {
            Ok(documents)
        } else {
            Err(format!(
                "the documents given for the ECU definition {id} do not all carry its identifier"
            ))
        }
    };
    // A rule reads one of the vehicle's own ECUs (ADR-326 item 2): a definition an entry names.
    let alternatives: HashSet<&str> = tables
        .entry_definitions
        .iter()
        .map(|row| row.definition.as_str())
        .collect();
    for row in &tables.entry_definitions {
        if let Err(hint) = named(&row.definition) {
            errors.push(
                ErrorCode::UnknownReference,
                ENTRY_DEFINITIONS.file,
                Some(row.line),
                Some("definition"),
                hint,
            );
        }
    }
    let sources = tables
        .vin_rules
        .iter()
        .map(|row| (VIN_RULES.file, row.line, &row.source, true))
        .chain(
            tables
                .field_rules
                .iter()
                .map(|row| (FIELD_RULES.file, row.line, &row.source, false)),
        );
    for (file, line, source, vin) in sources {
        let mut report = |code: ErrorCode, column: &str, hint: String| {
            errors.push(code, file, Some(line), Some(column), hint);
        };
        if !alternatives.contains(source.definition.as_str()) {
            report(
                ErrorCode::UnknownReference,
                "definition",
                format!(
                    "{} is not an ECU definition of this vehicle definition's entries",
                    source.definition
                ),
            );
            continue;
        }
        let documents = match named(&source.definition) {
            Ok(documents) => documents,
            Err(hint) => {
                report(ErrorCode::UnknownReference, "definition", hint);
                continue;
            }
        };
        let Some(document) = documents.iter().find(|document| {
            document
                .variant
                .as_ref()
                .and_then(|variant| variant.short_name.as_deref())
                == Some(source.variant.as_str())
        }) else {
            report(
                ErrorCode::UnknownReference,
                "variant",
                format!("{} has no variant {}", source.definition, source.variant),
            );
            continue;
        };
        let variant = document.variant.as_ref().expect("found by its variant");
        let Some(service) = variant
            .services
            .iter()
            .flatten()
            .find(|service| service.short_name.as_deref() == Some(source.service.as_str()))
        else {
            report(
                ErrorCode::UnknownReference,
                "service",
                format!(
                    "variant {} has no service {}",
                    source.variant, source.service
                ),
            );
            continue;
        };
        if !read_only(variant, service) {
            report(
                ErrorCode::BadValue,
                "service",
                format!(
                    "{} is not a read-only service of its protocol's application layer",
                    source.service
                ),
            );
            continue;
        }
        // The read is sent as declared: a request that needs a value has none to send, as for
        // an identity source (`csv-tables.md` 4.13).
        if service
            .request
            .as_ref()
            .and_then(|request| request.params.as_ref())
            .is_some_and(|params| !params.is_empty())
        {
            report(
                ErrorCode::BadValue,
                "service",
                format!(
                    "{} needs a value in its request, which a rule cannot give",
                    source.service
                ),
            );
            continue;
        }
        let Some(field) = top_field(service, &source.field) else {
            report(
                ErrorCode::UnknownReference,
                "field",
                format!(
                    "{} has no field {} at the top of its positive response",
                    source.service, source.field
                ),
            );
            continue;
        };
        if !matches!(field.shape, ShapeT::NONE) {
            report(
                ErrorCode::BadValue,
                "field",
                format!(
                    "{} is shaped (a repeat, selection or group), not a value a rule can compare",
                    source.field
                ),
            );
        } else if vin && !matches!(field.kind, DataKind::Ascii | DataKind::Utf8) {
            report(
                ErrorCode::BadValue,
                "field",
                format!("{} is not text, so it cannot carry a VIN", source.field),
            );
        }
    }
}

/// Whether `service` is in the read-only set of the application layer every protocol of
/// `variant` names (ADR-309 item 3, ADR-327 item 1).
fn read_only(variant: &diag_ir::declaration::EcuVariantT, service: &ServiceT) -> bool {
    let mut layers = variant
        .protocols
        .iter()
        .flatten()
        .map(|protocol| protocol.application_layer.as_deref());
    let Some(Some(layer)) = layers.next() else {
        return false;
    };
    if !layers.all(|other| other == Some(layer)) {
        return false;
    }
    let Some(sid) = service.request.as_ref().map(|request| request.sid) else {
        return false;
    };
    diag_ir::read_only_services(layer).is_some_and(|services| services.contains(&sid))
}

/// The field named `name` at the top of `service`'s positive response.
fn top_field<'a>(service: &'a ServiceT, name: &str) -> Option<&'a FieldT> {
    service
        .positive
        .as_ref()?
        .fields
        .iter()
        .flatten()
        .find(|field| field.short_name.as_deref() == Some(name))
}

#[cfg(test)]
mod tests;

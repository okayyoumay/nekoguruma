//! Reading the CSV tables of a definition directory (`docs/csv-tables.md`, ADR-295).
//!
//! [`read_directory`] (or [`read_files`] for files already in memory) checks the file set, the
//! dialect, the value syntax, and each table's columns and cells, and returns the typed rows of
//! every table. Every error is collected and reported at once, sorted as section 7 of the format
//! describes. The rules that compare rows with each other or with other tables (references,
//! variant scoping, the field rules of section 5) are not checked yet, apart from names defined
//! twice in one table.

mod error;
mod records;
pub mod rows;
mod sheet;
mod values;

use std::collections::HashMap;
use std::hash::Hash;
use std::path::Path;

use error::Errors;
pub use error::{ErrorCode, Rejected, TableError};
use rows::*;
use sheet::{Sheet, Spec};
pub use values::{Generated, ServiceName};

/// The typed rows of every table of a definition directory, in row order. An absent table is
/// empty.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tables {
    pub variants: Vec<VariantRow>,
    pub variant_patterns: Vec<VariantPatternRow>,
    pub protocols: Vec<ProtocolRow>,
    pub comparams: Vec<ComParamRow>,
    pub texts: Vec<CatalogRow>,
    pub units: Vec<CatalogRow>,
    pub conversions: Vec<ConversionRow>,
    pub conversion_scales: Vec<ConversionScaleRow>,
    pub services: Vec<ServiceRow>,
    pub dids: Vec<DidRow>,
    pub routines: Vec<RoutineRow>,
    pub nrcs: Vec<NrcRow>,
    pub layouts: Vec<LayoutRow>,
    pub fields: Vec<FieldRow>,
    pub repeats: Vec<RepeatRow>,
    pub select_cases: Vec<SelectCaseRow>,
    pub dtcs: Vec<DtcRow>,
    pub flash_sessions: Vec<FlashSessionRow>,
    pub flash_segments: Vec<FlashSegmentRow>,
}

/// Why [`read_directory`] failed.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The directory or one of its table files could not be read.
    #[error("cannot read the definition directory: {0}")]
    Io(#[from] std::io::Error),
    /// The tables break the format.
    #[error("{0}")]
    Rejected(#[from] Rejected),
}

/// Every table of section 1, in the order of its table.
const SPECS: [&Spec; 19] = [
    &VARIANTS,
    &VARIANT_PATTERNS,
    &PROTOCOLS,
    &COMPARAMS,
    &TEXTS,
    &UNITS,
    &CONVERSIONS,
    &CONVERSION_SCALES,
    &SERVICES,
    &DIDS,
    &ROUTINES,
    &NRCS,
    &LAYOUTS,
    &FIELDS,
    &REPEATS,
    &SELECT_CASES,
    &DTCS,
    &FLASH_SESSIONS,
    &FLASH_SEGMENTS,
];

/// The tables a definition must have.
const REQUIRED_FILES: [&str; 2] = ["variants.csv", "protocols.csv"];

/// Reads the tables of the definition directory `dir`. Only the files directly in it are
/// considered; files whose names end neither in `.csv` nor in `.star` are not read.
pub fn read_directory(dir: &Path) -> Result<Tables, ReadError> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // Only a table is read; any other name is judged by its name alone.
        let contents = if SPECS.iter().any(|spec| spec.file == name) {
            std::fs::read(entry.path())?
        } else {
            Vec::new()
        };
        files.push((name, contents));
    }
    Ok(read_files(files)?)
}

/// Reads the tables from `files`, the names and contents of the files directly in a definition
/// directory.
pub fn read_files(files: impl IntoIterator<Item = (String, Vec<u8>)>) -> Result<Tables, Rejected> {
    let mut errors = Errors::default();
    let mut sheets: HashMap<&'static str, Sheet> = HashMap::new();
    for (name, contents) in files {
        let lower = name.to_ascii_lowercase();
        let csv = lower.ends_with(".csv");
        if !csv && !lower.ends_with(".star") {
            continue;
        }
        if !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        {
            errors.push(
                ErrorCode::UnknownFile,
                &name,
                None,
                None,
                "a .csv or .star file name may use only A-Z, a-z, 0-9, _, - and .",
            );
            continue;
        }
        if !csv {
            continue;
        }
        let Some(spec) = SPECS.iter().find(|spec| spec.file == name) else {
            errors.push(
                ErrorCode::UnknownFile,
                &name,
                None,
                None,
                format!("{name} is not a table of the format"),
            );
            continue;
        };
        match records::parse(&contents) {
            Ok(records) => {
                sheets.insert(spec.file, Sheet::new(spec, records, &mut errors));
            }
            Err(syntax) => {
                errors.push(
                    ErrorCode::Syntax,
                    spec.file,
                    Some(syntax.line),
                    None,
                    syntax.hint,
                );
                // The rest of the file cannot be trusted; it is read as empty.
                sheets.insert(spec.file, Sheet::new(spec, Vec::new(), &mut errors));
            }
        }
    }
    for file in REQUIRED_FILES {
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
    if sheets.get("variants.csv").is_some_and(Sheet::is_empty) {
        errors.push(
            ErrorCode::MissingColumn,
            "variants.csv",
            None,
            None,
            "a definition needs at least one variant",
        );
    }
    let empty = |spec: &Spec| Sheet::new(spec, Vec::new(), &mut Errors::default());
    let mut sheet = |spec: &Spec| sheets.remove(spec.file).unwrap_or_else(|| empty(spec));
    let e = &mut errors;
    let tables = Tables {
        variants: variants(&sheet(&VARIANTS), e),
        variant_patterns: variant_patterns(&sheet(&VARIANT_PATTERNS), e),
        protocols: protocols(&sheet(&PROTOCOLS), e),
        comparams: comparams(&sheet(&COMPARAMS), e),
        texts: catalog(&sheet(&TEXTS), e, "text"),
        units: catalog(&sheet(&UNITS), e, "symbol"),
        conversions: conversions(&sheet(&CONVERSIONS), e),
        conversion_scales: conversion_scales(&sheet(&CONVERSION_SCALES), e),
        services: services(&sheet(&SERVICES), e),
        dids: dids(&sheet(&DIDS), e),
        routines: routines(&sheet(&ROUTINES), e),
        nrcs: nrcs(&sheet(&NRCS), e),
        layouts: layouts(&sheet(&LAYOUTS), e),
        fields: fields(&sheet(&FIELDS), e),
        repeats: repeats(&sheet(&REPEATS), e),
        select_cases: select_cases(&sheet(&SELECT_CASES), e),
        dtcs: dtcs(&sheet(&DTCS), e),
        flash_sessions: flash_sessions(&sheet(&FLASH_SESSIONS), e),
        flash_segments: flash_segments(&sheet(&FLASH_SEGMENTS), e),
    };
    check_duplicates(&tables, e);
    if errors.0.is_empty() {
        Ok(tables)
    } else {
        Err(Rejected::new(errors.0))
    }
}

/// Reports every row that defines again a name, id or key an earlier row of the same table
/// defines (section 3; `name` and its container for fields, 5.1).
fn check_duplicates(tables: &Tables, errors: &mut Errors) {
    fn once<K: Eq + Hash>(
        errors: &mut Errors,
        file: &str,
        column: &str,
        rows: impl IntoIterator<Item = (Line, K, String)>,
    ) {
        let mut first: HashMap<K, Line> = HashMap::new();
        for (line, key, shown) in rows {
            match first.get(&key) {
                Some(&earlier) => errors.push(
                    ErrorCode::Duplicate,
                    file,
                    Some(line),
                    Some(column),
                    format!("{shown} is already defined on line {earlier}"),
                ),
                None => {
                    first.insert(key, line);
                }
            }
        }
    }
    let t = tables;
    let by_name = |line, name: &String| (line, name.clone(), name.clone());
    let by_id = |line, id: u32| (line, id, id.to_string());
    once(
        errors,
        "variants.csv",
        "id",
        t.variants.iter().map(|r| by_id(r.line, r.id)),
    );
    once(
        errors,
        "variants.csv",
        "name",
        t.variants.iter().map(|r| by_name(r.line, &r.name)),
    );
    for (file, rows) in [("texts.csv", &t.texts), ("units.csv", &t.units)] {
        once(
            errors,
            file,
            "key",
            rows.iter().map(|r| by_name(r.line, &r.key)),
        );
        once(errors, file, "id", rows.iter().map(|r| by_id(r.line, r.id)));
    }
    once(
        errors,
        "conversions.csv",
        "name",
        t.conversions.iter().map(|r| by_name(r.line, &r.name)),
    );
    once(
        errors,
        "services.csv",
        "name",
        t.services.iter().map(|r| by_name(r.line, &r.name)),
    );
    once(
        errors,
        "dids.csv",
        "name",
        t.dids.iter().map(|r| by_name(r.line, &r.name)),
    );
    once(
        errors,
        "routines.csv",
        "name",
        t.routines.iter().map(|r| by_name(r.line, &r.name)),
    );
    once(
        errors,
        "layouts.csv",
        "name",
        t.layouts.iter().map(|r| by_name(r.line, &r.name)),
    );
    once(
        errors,
        "flash_sessions.csv",
        "name",
        t.flash_sessions.iter().map(|r| by_name(r.line, &r.name)),
    );
    once(
        errors,
        "fields.csv",
        "name",
        t.fields.iter().map(|r| {
            (
                r.line,
                (r.container.clone(), r.name.clone()),
                format!("field {}", r.name),
            )
        }),
    );
    once(
        errors,
        "repeats.csv",
        "field",
        t.repeats.iter().map(|r| {
            (
                r.line,
                (r.container.clone(), r.field.clone()),
                format!("the count of {}", r.field),
            )
        }),
    );
    once(
        errors,
        "select_cases.csv",
        "case",
        t.select_cases.iter().map(|r| {
            (
                r.line,
                (r.container.clone(), r.field.clone(), r.case.clone()),
                format!("case {} of {}", r.case, r.field),
            )
        }),
    );
    once(
        errors,
        "flash_segments.csv",
        "name",
        t.flash_segments.iter().map(|r| {
            (
                r.line,
                (r.session.clone(), r.name.clone()),
                format!("segment {} of {}", r.name, r.session),
            )
        }),
    );
}

#[cfg(test)]
mod tests;

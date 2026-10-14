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

/// Reads the tables of the definition directory `dir`. Only the entries directly in it are
/// considered, following symbolic links; entries whose names end neither in `.csv` nor in
/// `.star` are not read, and one with such a name that is not a regular file is rejected.
pub fn read_directory(dir: &Path) -> Result<Tables, ReadError> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // `metadata` follows a link; a link to nothing is not a regular file.
        let is_file = match std::fs::metadata(entry.path()) {
            Ok(metadata) => metadata.is_file(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        // Only a table is read; any other name is judged by its name alone.
        let contents = if !is_file {
            None
        } else if SPECS.iter().any(|spec| spec.file == name) {
            Some(std::fs::read(entry.path())?)
        } else {
            Some(Vec::new())
        };
        entries.push((name, contents));
    }
    Ok(read_entries(entries)?)
}

/// Reads the tables from `files`, the names and contents of the files directly in a definition
/// directory.
pub fn read_files(files: impl IntoIterator<Item = (String, Vec<u8>)>) -> Result<Tables, Rejected> {
    read_entries(
        files
            .into_iter()
            .map(|(name, contents)| (name, Some(contents))),
    )
}

/// Reads the tables from a directory's entries; `None` contents mark an entry that is not a
/// regular file.
fn read_entries(
    entries: impl IntoIterator<Item = (String, Option<Vec<u8>>)>,
) -> Result<Tables, Rejected> {
    let mut errors = Errors::default();
    let mut sheets: HashMap<&'static str, Sheet> = HashMap::new();
    // Tables that broke the record format: their other errors are not reported (section 7).
    let mut broken: Vec<&'static str> = Vec::new();
    for (name, contents) in entries {
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
                // The rest of the file cannot be trusted; it is read as empty.
                broken.push(spec.file);
                Sheet::new(spec, Vec::new(), &mut errors)
            }
        };
        if sheet.had_syntax_error {
            broken.push(spec.file);
        }
        sheets.insert(spec.file, sheet);
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
    if sheets.get("variants.csv").is_some_and(Sheet::is_empty) && !broken.contains(&"variants.csv")
    {
        errors.push(
            ErrorCode::MissingColumn,
            "variants.csv",
            None,
            None,
            "a definition needs at least one variant",
        );
    }
    for spec in SPECS {
        sheets
            .entry(spec.file)
            .or_insert_with(|| Sheet::new(spec, Vec::new(), &mut Errors::default()));
    }
    let sheet = |spec: &Spec| &sheets[spec.file];
    let e = &mut errors;
    let tables = Tables {
        variants: variants(sheet(&VARIANTS), e),
        variant_patterns: variant_patterns(sheet(&VARIANT_PATTERNS), e),
        protocols: protocols(sheet(&PROTOCOLS), e),
        comparams: comparams(sheet(&COMPARAMS), e),
        texts: catalog(sheet(&TEXTS), e, "text"),
        units: catalog(sheet(&UNITS), e, "symbol"),
        conversions: conversions(sheet(&CONVERSIONS), e),
        conversion_scales: conversion_scales(sheet(&CONVERSION_SCALES), e),
        services: services(sheet(&SERVICES), e),
        dids: dids(sheet(&DIDS), e),
        routines: routines(sheet(&ROUTINES), e),
        nrcs: nrcs(sheet(&NRCS), e),
        layouts: layouts(sheet(&LAYOUTS), e),
        fields: fields(sheet(&FIELDS), e),
        repeats: repeats(sheet(&REPEATS), e),
        select_cases: select_cases(sheet(&SELECT_CASES), e),
        dtcs: dtcs(sheet(&DTCS), e),
        flash_sessions: flash_sessions(sheet(&FLASH_SESSIONS), e),
        flash_segments: flash_segments(sheet(&FLASH_SEGMENTS), e),
    };
    check_duplicates(&sheets, e);
    if errors.0.is_empty() {
        Ok(tables)
    } else {
        Err(Rejected::new(errors.0))
    }
}

/// A key that no two rows of a table may share (section 3; a field's name per container, 5.1;
/// a case per `select` field and a count per `repeat` field, 5.4; a segment per session, 4.12).
struct UniqueKey {
    file: &'static str,
    /// The cells that make up the key; the last is the one reported.
    columns: &'static [&'static str],
    /// Whether the key is a number, compared by value rather than as written.
    numeric: bool,
}

const UNIQUE_KEYS: [UniqueKey; 16] = [
    UniqueKey {
        file: "variants.csv",
        columns: &["id"],
        numeric: true,
    },
    UniqueKey {
        file: "variants.csv",
        columns: &["name"],
        numeric: false,
    },
    UniqueKey {
        file: "texts.csv",
        columns: &["key"],
        numeric: false,
    },
    UniqueKey {
        file: "texts.csv",
        columns: &["id"],
        numeric: true,
    },
    UniqueKey {
        file: "units.csv",
        columns: &["key"],
        numeric: false,
    },
    UniqueKey {
        file: "units.csv",
        columns: &["id"],
        numeric: true,
    },
    UniqueKey {
        file: "conversions.csv",
        columns: &["name"],
        numeric: false,
    },
    UniqueKey {
        file: "services.csv",
        columns: &["name"],
        numeric: false,
    },
    UniqueKey {
        file: "dids.csv",
        columns: &["name"],
        numeric: false,
    },
    UniqueKey {
        file: "routines.csv",
        columns: &["name"],
        numeric: false,
    },
    UniqueKey {
        file: "layouts.csv",
        columns: &["name"],
        numeric: false,
    },
    UniqueKey {
        file: "flash_sessions.csv",
        columns: &["name"],
        numeric: false,
    },
    UniqueKey {
        file: "fields.csv",
        columns: &["container", "name"],
        numeric: false,
    },
    UniqueKey {
        file: "repeats.csv",
        columns: &["container", "field"],
        numeric: false,
    },
    UniqueKey {
        file: "select_cases.csv",
        columns: &["container", "field", "case"],
        numeric: false,
    },
    UniqueKey {
        file: "flash_segments.csv",
        columns: &["session", "name"],
        numeric: false,
    },
];

/// Reports every row that defines again a name, id or key an earlier row of the same table
/// defines. The keys are compared as written (numbers by value), so a row whose other cells
/// are wrong is still checked; a key with an empty or unparsable cell is left to the cell's own
/// error.
fn check_duplicates(sheets: &HashMap<&'static str, Sheet>, errors: &mut Errors) {
    for key in &UNIQUE_KEYS {
        let column = key.columns[key.columns.len() - 1];
        let mut first: HashMap<Vec<String>, u32> = HashMap::new();
        for (line, cells) in sheets[key.file].cells(key.columns) {
            let Some(cells) = cells.into_iter().collect::<Option<Vec<&str>>>() else {
                continue;
            };
            let value: Vec<String> = if key.numeric {
                match values::integer(cells[0], false, 0, u64::MAX.into()) {
                    Ok(n) => vec![n.to_string()],
                    Err(_) => continue,
                }
            } else {
                cells.iter().map(|c| (*c).to_owned()).collect()
            };
            match first.get(&value) {
                Some(&earlier) => errors.push(
                    ErrorCode::Duplicate,
                    key.file,
                    Some(line),
                    Some(column),
                    format!(
                        "{} {} is already defined on line {earlier}",
                        column,
                        cells.join(" / ")
                    ),
                ),
                None => {
                    first.insert(value, line);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;

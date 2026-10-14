//! A table's header and the typed reading of its cells (`docs/csv-tables.md` section 2).

use std::collections::HashMap;

use super::error::{ErrorCode, Errors};
use super::records::Record;
use super::values::Parsed;

/// A table's file and columns. Every table also takes a `note` column, and a scoped table a
/// `variants` column.
pub(crate) struct Spec {
    pub(crate) file: &'static str,
    pub(crate) scoped: bool,
    /// The columns other than `note` and `variants`: name and whether it is required.
    pub(crate) columns: &'static [(&'static str, bool)],
}

/// A table's rows, with the header that locates their cells.
pub(crate) struct Sheet {
    file: &'static str,
    header: HashMap<&'static str, usize>,
    records: Vec<Record>,
}

impl Sheet {
    /// Checks the header and the cell count of every row, skipping blank rows. A file without
    /// records is an empty table.
    pub(crate) fn new(spec: &Spec, records: Vec<Record>, errors: &mut Errors) -> Self {
        let mut records = records.into_iter().filter(|r| !r.is_blank());
        let mut header = HashMap::new();
        let Some(first) = records.next() else {
            return Self {
                file: spec.file,
                header,
                records: Vec::new(),
            };
        };
        let known = |name: &str| -> Option<&'static str> {
            spec.columns
                .iter()
                .map(|(c, _)| *c)
                .chain(["note"])
                .chain(spec.scoped.then_some("variants"))
                .find(|c| *c == name)
        };
        for (index, name) in first.cells.iter().enumerate() {
            match known(name) {
                Some(column) if header.insert(column, index).is_some() => errors.push(
                    ErrorCode::UnknownColumn,
                    spec.file,
                    Some(first.line),
                    Some(name),
                    format!("column {name} appears twice in the header"),
                ),
                Some(_) => {}
                None => errors.push(
                    ErrorCode::UnknownColumn,
                    spec.file,
                    Some(first.line),
                    Some(name),
                    format!("{} has no column {name:?}", spec.file),
                ),
            }
        }
        for (column, required) in spec.columns {
            if *required && !header.contains_key(column) {
                errors.push(
                    ErrorCode::MissingColumn,
                    spec.file,
                    Some(first.line),
                    Some(column),
                    format!("the required column {column} is missing from the header"),
                );
            }
        }
        let width = first.cells.len();
        let records = records
            .filter(|record| {
                let fits = record.cells.len() == width;
                if !fits {
                    errors.push(
                        ErrorCode::Syntax,
                        spec.file,
                        Some(record.line),
                        None,
                        format!(
                            "the row has {} cells, the header {width}",
                            record.cells.len()
                        ),
                    );
                }
                fits
            })
            .collect();
        Self {
            file: spec.file,
            header,
            records,
        }
    }

    /// Reads every row with `read`, which returns `None` for a row with an error.
    pub(crate) fn rows<T>(
        &self,
        errors: &mut Errors,
        mut read: impl FnMut(&mut Row<'_>, &mut Errors) -> Option<T>,
    ) -> Vec<T> {
        self.records
            .iter()
            .filter_map(|record| {
                let mut row = Row {
                    file: self.file,
                    line: record.line,
                    cells: &record.cells,
                    header: &self.header,
                    ok: true,
                };
                let value = read(&mut row, errors);
                if row.ok { value } else { None }
            })
            .collect()
    }

    /// Whether the table has no rows.
    pub(crate) fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// One row being read. Every failed cell is reported and marks the row as failed, so that a
/// table's reader can read all of a row's cells and report every error before giving up on it.
pub(crate) struct Row<'a> {
    pub(crate) file: &'static str,
    pub(crate) line: u32,
    cells: &'a [String],
    header: &'a HashMap<&'static str, usize>,
    ok: bool,
}

impl Row<'_> {
    /// The cell of `column`, or `None` when it is empty or the header leaves it out.
    pub(crate) fn cell(&self, column: &str) -> Option<&str> {
        self.header
            .get(column)
            .map(|&i| self.cells[i].as_str())
            .filter(|cell| !cell.is_empty())
    }

    /// Reports an error at `column` of this row and marks the row as failed.
    pub(crate) fn error(
        &mut self,
        errors: &mut Errors,
        code: ErrorCode,
        column: &str,
        hint: impl Into<String>,
    ) {
        self.ok = false;
        errors.push(code, self.file, Some(self.line), Some(column), hint);
    }

    /// A required cell. An empty one is reported, unless the column is missing from the header,
    /// which the header check reported already.
    pub(crate) fn required<T>(
        &mut self,
        errors: &mut Errors,
        column: &str,
        parse: impl FnOnce(&str) -> Parsed<T>,
    ) -> Option<T> {
        match self.cell(column) {
            Some(cell) => self.parse(errors, column, cell.to_owned(), parse),
            None => {
                if self.header.contains_key(column) {
                    self.error(
                        errors,
                        ErrorCode::MissingColumn,
                        column,
                        format!("{column} is required"),
                    );
                } else {
                    self.ok = false;
                }
                None
            }
        }
    }

    /// An optional cell: `Ok(None)` when empty, `Err` when it does not parse.
    pub(crate) fn optional<T>(
        &mut self,
        errors: &mut Errors,
        column: &str,
        parse: impl FnOnce(&str) -> Parsed<T>,
    ) -> Result<Option<T>, ()> {
        match self.cell(column) {
            None => Ok(None),
            Some(cell) => self
                .parse(errors, column, cell.to_owned(), parse)
                .map(Some)
                .ok_or(()),
        }
    }

    /// An optional cell with a default for an empty one; `None` when it does not parse.
    pub(crate) fn or<T>(
        &mut self,
        errors: &mut Errors,
        column: &str,
        default: T,
        parse: impl FnOnce(&str) -> Parsed<T>,
    ) -> Option<T> {
        self.optional(errors, column, parse)
            .ok()
            .map(|v| v.unwrap_or(default))
    }

    /// Reports `column` if it is set although the row's kind does not use it.
    pub(crate) fn unused(&mut self, errors: &mut Errors, column: &str, kind: &str) {
        if self.cell(column).is_some() {
            self.error(
                errors,
                ErrorCode::BadValue,
                column,
                format!("{column} is not used by {kind} and must be empty"),
            );
        }
    }

    fn parse<T>(
        &mut self,
        errors: &mut Errors,
        column: &str,
        cell: String,
        parse: impl FnOnce(&str) -> Parsed<T>,
    ) -> Option<T> {
        match parse(&cell) {
            Ok(value) => Some(value),
            Err(hint) => {
                self.error(errors, ErrorCode::BadValue, column, hint);
                None
            }
        }
    }
}

//! Errors of the table conversion (`docs/csv-tables.md` section 7).

use serde::Serialize;

/// The error codes of `docs/csv-tables.md` section 7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum ErrorCode {
    /// The file breaks the record format, is not UTF-8, or a row has the wrong number of cells.
    #[serde(rename = "CSV_SYNTAX")]
    Syntax,
    /// A `.csv` file that is not a table, a file name outside the allowed characters, or a
    /// required table that is missing.
    #[serde(rename = "CSV_UNKNOWN_FILE")]
    UnknownFile,
    /// A header name the table does not define, or a duplicate header name.
    #[serde(rename = "CSV_UNKNOWN_COLUMN")]
    UnknownColumn,
    /// A required column missing from the header, a required cell left empty, or a row a rule
    /// requires that is missing.
    #[serde(rename = "CSV_MISSING_COLUMN")]
    MissingColumn,
    /// A value that does not parse, is out of range, or is set where its row's kind does not use
    /// it.
    #[serde(rename = "CSV_BAD_VALUE")]
    BadValue,
    /// A name, key or container that is not defined.
    #[serde(rename = "CSV_UNKNOWN_REFERENCE")]
    UnknownReference,
    /// A name, id or key defined twice.
    #[serde(rename = "CSV_DUPLICATE")]
    Duplicate,
    /// A construct this version of the format does not accept.
    #[serde(rename = "CSV_UNSUPPORTED")]
    Unsupported,
    /// A rule of `ir.fbs` is violated.
    #[serde(rename = "CSV_IR_RULE")]
    IrRule,
}

impl ErrorCode {
    /// The code as the format writes it, such as `CSV_BAD_VALUE`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "CSV_SYNTAX",
            Self::UnknownFile => "CSV_UNKNOWN_FILE",
            Self::UnknownColumn => "CSV_UNKNOWN_COLUMN",
            Self::MissingColumn => "CSV_MISSING_COLUMN",
            Self::BadValue => "CSV_BAD_VALUE",
            Self::UnknownReference => "CSV_UNKNOWN_REFERENCE",
            Self::Duplicate => "CSV_DUPLICATE",
            Self::Unsupported => "CSV_UNSUPPORTED",
            Self::IrRule => "CSV_IR_RULE",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One error, located as precisely as the format allows: an error about a whole file has no
/// line, one about the header has line 1, and a syntax error has no column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableError {
    /// The error code.
    pub code: ErrorCode,
    /// The file's name in the definition directory.
    pub file: String,
    /// The line on which the record starts, counting the header as line 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// The column's header name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// What is wrong, for a person.
    pub hint: String,
}

impl std::fmt::Display for TableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.file)?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
        }
        if let Some(column) = &self.column {
            write!(f, " [{column}]")?;
        }
        write!(f, ": {}: {}", self.code, self.hint)
    }
}

/// Why a definition directory was rejected: every error found, sorted by file name, line and
/// column (`docs/csv-tables.md` section 7). A missing line or column sorts first, and columns
/// sort by their header names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rejected {
    /// Always `"rejected"`, as in the report format of section 7.
    pub status: &'static str,
    /// The errors, in report order.
    pub errors: Vec<TableError>,
}

impl Rejected {
    pub(crate) fn new(mut errors: Vec<TableError>) -> Self {
        // A stable sort keeps the order in which one cell's errors were found.
        errors.sort_by(|a, b| {
            (
                a.file.as_bytes(),
                a.line,
                a.column.as_deref().map(str::as_bytes),
            )
                .cmp(&(
                    b.file.as_bytes(),
                    b.line,
                    b.column.as_deref().map(str::as_bytes),
                ))
        });
        Self {
            status: "rejected",
            errors,
        }
    }
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "definition rejected with {} error(s)", self.errors.len())?;
        for error in &self.errors {
            write!(f, "\n{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Rejected {}

/// Collects errors while a directory is read.
#[derive(Debug, Default)]
pub(crate) struct Errors(pub(crate) Vec<TableError>);

impl Errors {
    pub(crate) fn push(
        &mut self,
        code: ErrorCode,
        file: &str,
        line: Option<u32>,
        column: Option<&str>,
        hint: impl Into<String>,
    ) {
        self.0.push(TableError {
            code,
            file: file.to_owned(),
            line,
            column: column.map(str::to_owned),
            hint: hint.into(),
        });
    }
}

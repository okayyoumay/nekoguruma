//! The fields that identity and precondition sources name (`docs/csv-tables.md` 4.13, 4.14,
//! ADR-336 item 1): the part of the field stage (ADR-298) that follows the references
//! `resolve.rs` checked. A source reads a field of a generated DID read the way the agent's
//! source table does (`crates/agent/src/inputs.rs`): the DID's only response field, so that it is
//! the whole data record after the echoed identifier, as an unsigned big-endian integer or
//! ASCII text.

use std::collections::HashMap;

use super::error::{ErrorCode, Errors};
use super::resolve::Definition;
use super::rows::*;
use super::values::ServiceName;

/// What a source needs its field to be.
#[derive(Clone, Copy)]
enum Want {
    /// A VIN: a fixed 17-byte ASCII text, since the agent compares a 17-character text.
    Vin,
    /// Another identity value: ASCII text or an unsigned integer.
    Identity,
    /// A precondition's value: an unsigned integer.
    Number,
}

/// The bits of a 17-byte VIN field.
const VIN_BITS: u32 = 17 * 8;

/// Checks the fields that the sources of `definition` name, adding every error found to
/// `errors`.
pub(super) fn check(definition: &Definition, errors: &mut Errors) {
    let t = &definition.tables;
    // The record of each DID: its fields in `fields.csv`.
    let mut records: HashMap<&str, Vec<&FieldRow>> = HashMap::new();
    for field in &t.fields {
        if let Container::Did(did) = &field.container {
            records.entry(did.as_str()).or_default().push(field);
        }
    }
    let mut checker = Checker { records, errors };
    for row in &t.identity {
        let want = if row.kind == diag_ir::IdentityKind::Vin {
            Want::Vin
        } else {
            Want::Identity
        };
        checker.field(
            "identity.csv",
            row.line,
            "field",
            &row.service,
            &row.field,
            want,
        );
    }
    for row in &t.preconditions {
        for (column, source) in [
            ("default_source", Some(&row.default_source)),
            ("programming_source", row.programming_source.as_ref()),
        ] {
            if let Some(SourceRef::Service { service, field }) = source {
                checker.field(
                    "preconditions.csv",
                    row.line,
                    column,
                    service,
                    field,
                    Want::Number,
                );
            }
        }
    }
}

struct Checker<'a> {
    records: HashMap<&'a str, Vec<&'a FieldRow>>,
    errors: &'a mut Errors,
}

impl Checker<'_> {
    fn field(
        &mut self,
        file: &str,
        line: Line,
        column: &str,
        service: &ServiceName,
        name: &str,
        want: Want,
    ) {
        let mut bad = |code: ErrorCode, hint: String| {
            self.errors.push(code, file, Some(line), Some(column), hint);
        };
        let record = self
            .records
            .get(service.name.as_str())
            .map_or(&[][..], Vec::as_slice);
        let Some(field) = record.iter().find(|f| f.name == name) else {
            bad(
                ErrorCode::UnknownReference,
                format!("the response of {}.read has no field {name}", service.name),
            );
            return;
        };
        if record.len() != 1 {
            bad(
                ErrorCode::BadValue,
                format!(
                    "{}.read has {} fields; a source needs a DID whose record is the one field, \
                     since the agent reads the whole record after the echoed identifier",
                    service.name,
                    record.len()
                ),
            );
            return;
        }
        if field.constant.is_some() || field.shape.is_some() {
            bad(
                ErrorCode::BadValue,
                format!("{name} is a constant or a shaped field, not a value the ECU reports"),
            );
            return;
        }
        let ty = field.field_type;
        let fixed = matches!(field.length, None | Some(Length::Fixed));
        let hint = match (want, ty) {
            (Want::Vin, Some(FieldType::Ascii)) if fixed && field.bits == Some(VIN_BITS) => None,
            (Want::Vin, _) => Some(format!(
                "{name} must be a fixed 17-byte ascii field (136 bits), since the agent compares \
                 the VIN as a 17-character text"
            )),
            (Want::Identity, Some(FieldType::Ascii)) => None,
            (Want::Identity | Want::Number, Some(FieldType::Uint)) => unsigned(field),
            (Want::Identity, _) => Some(format!(
                "{name} must be an ascii text or an unsigned integer, the encodings the agent \
                 decodes"
            )),
            (Want::Number, _) => Some(format!(
                "{name} must be an unsigned integer (uint), which the agent compares with the \
                 range"
            )),
        };
        if let Some(hint) = hint {
            bad(ErrorCode::BadValue, hint);
        }
    }
}

/// Why a `uint` field is not one the agent reads: it decodes whole bytes, at most eight, as a
/// big-endian integer.
fn unsigned(field: &FieldRow) -> Option<String> {
    let whole_bytes = field.bit == 0 && field.bits.is_some_and(|b| b % 8 == 0 && b <= 64);
    if !field.big_endian || !whole_bytes {
        return Some(format!(
            "{} must be a big-endian uint of whole bytes, 8 at most, since the agent reads an \
             unsigned big-endian integer",
            field.name
        ));
    }
    None
}

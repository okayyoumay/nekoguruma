//! Decoding a positive response through its flat fields (design 8.2.2, ADR-297).
//!
//! [`decode_positive_response`] checks a response's SID and prefix, extracts each field of
//! `PositiveResponse.fields` as ISO 22901-1 (2008) 7.3.6.3 describes, and applies the field's
//! conversion (`Compu`). [`decode_dtc_fields`] decodes one of a DTC's flat field lists the same
//! way, from a byte the caller states. Fields with a shape (nested layouts, ADR-242) and
//! `CompuBytecode` are not decoded here yet; they fail with [`DecodeError::Unsupported`].

use flatbuffers::{ForwardsUOffset, Vector};
use thiserror::Error;

use crate::declaration::{
    ByteOrder, Compu, DataKind, DtcDefinition, Field, LengthKind, Placement, PositiveResponse,
    Shape, Termination,
};

/// A decoded value. Before a conversion it is the field's internal value; after one, the
/// physical value: a float for every conversion except a text table, which keeps the internal
/// value and reports its text by id ([`DecodedField::text_id`]).
#[derive(Debug, Clone, PartialEq)]
pub enum PhysicalValue {
    /// A `Uint` or `Bcd` field (BCD as the number its digits spell).
    Unsigned(u64),
    /// An `Int` field, in two's complement.
    Signed(i64),
    /// A `Float` field, or the result of a numeric conversion.
    Float(f64),
    /// An `Ascii`, `Utf8` or `Unicode2` field.
    Text(String),
    /// A `Bytes` field.
    Bytes(Vec<u8>),
}

/// One decoded field of a response.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedField {
    /// `Field.id`.
    pub id: u32,
    /// `Field.short_name`, the key of the value in a Starlark response's `fields`.
    pub short_name: String,
    /// The physical value.
    pub value: PhysicalValue,
    /// The text a text table chose for the value (resolved on the server, design 9.2); 0 when
    /// the field has no text table.
    pub text_id: u32,
    /// `Field.unit_id`; 0 for none.
    pub unit_id: u32,
}

/// Why a response could not be decoded. Errors about one field carry its `Field.id`.
#[derive(Debug, Error, PartialEq)]
pub enum DecodeError {
    /// The message is empty, so it has no SID.
    #[error("the response is empty")]
    Empty,
    /// The message's first byte is not the declared positive response SID.
    #[error("response SID {found:#04x} is not the declared {expected:#04x}")]
    WrongSid {
        /// `PositiveResponse.sid`.
        expected: u8,
        /// The message's first byte.
        found: u8,
    },
    /// The bytes after the SID do not start with `PositiveResponse.prefix` (a sub-function or
    /// an echoed identifier, ADR-295).
    #[error("the response does not start with its declared prefix")]
    PrefixMismatch,
    /// The byte a DTC's field list is to start at is past the end of the message.
    #[error("the DTC fields start at byte {start}, past the end of the {len}-byte message")]
    StartPastEnd {
        /// The stated starting byte.
        start: usize,
        /// The message's length.
        len: usize,
    },
    /// The field reaches past the end of the message.
    #[error("field {field} reaches past the end of the response")]
    Truncated {
        /// The field.
        field: u32,
    },
    /// The message's bytes are not a value of the field's type: a BCD digit above 9, invalid
    /// UTF-8, a UTF-16 surrogate in a `Unicode2` field, an odd `Unicode2` length, or a length
    /// taken from another field that is negative, not whole or not a valid width.
    #[error("field {field}: {reason}")]
    InvalidValue {
        /// The field.
        field: u32,
        /// What is wrong.
        reason: &'static str,
    },
    /// The conversion has no result for the value: no scale or text entry matches, the value
    /// is outside an interpolation table, a denominator is zero, or the result is not finite.
    #[error("field {field}: conversion failed: {reason}")]
    Conversion {
        /// The field.
        field: u32,
        /// What is wrong.
        reason: &'static str,
    },
    /// The field breaks a rule of `ir.fbs` that a frontend must enforce, such as a width the
    /// type cannot have or a length kind the type does not take.
    #[error("field {field} is not a valid decode plan: {reason}")]
    InvalidPlan {
        /// The field.
        field: u32,
        /// What is wrong.
        reason: &'static str,
    },
    /// The field needs what this decoder does not do yet: a shape (nested layout) or a
    /// `CompuBytecode` conversion.
    #[error("field {field}: {what} is not supported")]
    Unsupported {
        /// The field.
        field: u32,
        /// What the field needs.
        what: &'static str,
    },
}

/// Decodes `message`, a whole positive response starting with its SID, through `response`'s
/// flat fields, in array order.
///
/// The SID must equal `PositiveResponse.sid` and the bytes after it must start with
/// `PositiveResponse.prefix`. Field positions count from the SID (`ir.fbs`, `Placement`); an
/// `AfterPrevious` first field starts after the prefix. Bytes after the last field are not
/// read and are not an error.
pub fn decode_positive_response(
    response: &PositiveResponse<'_>,
    message: &[u8],
) -> Result<Vec<DecodedField>, DecodeError> {
    let (&sid, rest) = message.split_first().ok_or(DecodeError::Empty)?;
    if sid != response.sid() {
        return Err(DecodeError::WrongSid {
            expected: response.sid(),
            found: sid,
        });
    }
    let prefix = response.prefix().map_or(&[][..], |p| p.bytes());
    if !rest.starts_with(prefix) {
        return Err(DecodeError::PrefixMismatch);
    }
    match response.fields() {
        Some(fields) => decode_fields(fields, message, 1 + prefix.len()),
        None => Ok(Vec::new()),
    }
}

/// One of a DTC's field lists (`DtcDefinition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtcFields {
    /// `status_fields`: the bits of the DTC's status byte.
    Status,
    /// `snapshot_fields`: the data of a snapshot (freeze frame) record.
    Snapshot,
    /// `extended_fields`: the data of an extended data record.
    Extended,
}

/// Decodes one of `dtc`'s field lists from `message`, with the list's layout starting at byte
/// `start` of it.
///
/// The fields are decoded as [`decode_positive_response`] decodes a response's, in array order,
/// but their positions count from `start`: an `Absolute` field at bit offset 0 begins at byte
/// `start`, and an `AfterPrevious` first field starts there. A field that reaches past the end
/// of `message` is truncated, and a terminated field ends at the end of `message` at the
/// latest; bytes after the last field are not read. Where a list starts depends on the service
/// that returned the DTC (its status byte in a status record, the data after a record's
/// header in a snapshot or extended record), so the caller states it. A list the definition
/// does not have decodes to no fields.
pub fn decode_dtc_fields(
    dtc: &DtcDefinition<'_>,
    which: DtcFields,
    message: &[u8],
    start: usize,
) -> Result<Vec<DecodedField>, DecodeError> {
    let fields = match which {
        DtcFields::Status => dtc.status_fields(),
        DtcFields::Snapshot => dtc.snapshot_fields(),
        DtcFields::Extended => dtc.extended_fields(),
    };
    let Some(fields) = fields.filter(|f| !f.is_empty()) else {
        return Ok(Vec::new());
    };
    let rest = message.get(start..).ok_or(DecodeError::StartPastEnd {
        start,
        len: message.len(),
    })?;
    decode_fields(fields, rest, 0)
}

/// Decodes a flat field array over `message`; `start` is the byte where an `AfterPrevious`
/// first field starts.
fn decode_fields(
    fields: Vector<'_, ForwardsUOffset<Field<'_>>>,
    message: &[u8],
    start: usize,
) -> Result<Vec<DecodedField>, DecodeError> {
    // Not reserved from fields.len(): the first field may fail at once.
    let mut decoded: Vec<DecodedField> = Vec::new();
    // The byte edge after the previous field.
    let mut edge = start;
    for field in fields {
        let id = field.id();
        if field.shape_type() != Shape::NONE {
            return Err(DecodeError::Unsupported {
                field: id,
                what: "a nested layout (shape)",
            });
        }
        let (byte, bit) = position(&field, edge)?;
        let (internal, end) = extract(&field, message, byte, bit, &decoded)?;
        edge = end;
        let (value, text_id) = convert(&field, internal)?;
        decoded.push(DecodedField {
            id,
            short_name: field.short_name().unwrap_or_default().to_owned(),
            value,
            text_id,
            unit_id: field.unit_id(),
        });
    }
    Ok(decoded)
}

/// The byte and the bit position (0 to 7, from the least significant bit) where a field starts.
fn position(field: &Field<'_>, edge: usize) -> Result<(usize, u32), DecodeError> {
    let offset = field.bit_offset();
    match field.placement() {
        Placement::Absolute => Ok((as_usize(offset / 8), offset % 8)),
        Placement::AfterPrevious if offset <= 7 => Ok((edge, offset)),
        Placement::AfterPrevious => Err(plan(field, "an AfterPrevious bit position above 7")),
        _ => Err(plan(field, "an unknown placement")),
    }
}

/// Extracts a field's internal value; returns it with the byte edge after the field.
fn extract(
    field: &Field<'_>,
    message: &[u8],
    byte: usize,
    bit: u32,
    decoded: &[DecodedField],
) -> Result<(PhysicalValue, usize), DecodeError> {
    let kind = field.kind();
    let string_like = is_string_like(kind);
    match field.length_kind() {
        LengthKind::Fixed => extract_with_width(field, message, byte, bit, field.bit_length()),
        LengthKind::FromField => {
            let width = width_from_field(field, decoded)?;
            extract_with_width(field, message, byte, bit, width)
        }
        LengthKind::LeadingLength if string_like => {
            let prefix_bits = field.bit_length();
            if !(1..=32).contains(&prefix_bits) {
                return Err(plan(field, "a leading length prefix outside 1 to 32 bits"));
            }
            let len = extract_bits(field, message, byte, bit, prefix_bits)?;
            let start = add(field, byte, byte_count(bit, prefix_bits))?;
            let end = add(field, start, as_usize_u64(field, len)?)?;
            let bytes = message.get(start..end).ok_or(truncated(field))?;
            Ok((string_value(field, bytes)?, end))
        }
        LengthKind::Terminated if string_like => {
            if bit != 0 {
                return Err(plan(
                    field,
                    "a terminated value that does not start at a byte edge",
                ));
            }
            extract_terminated(field, message, byte)
        }
        LengthKind::LeadingLength | LengthKind::Terminated => Err(plan(
            field,
            "a leading length or termination on a type that is not bytes or a string",
        )),
        _ => Err(plan(field, "an unknown length kind")),
    }
}

/// Extracts a field `width` bits wide (a `Fixed` or `FromField` length).
fn extract_with_width(
    field: &Field<'_>,
    message: &[u8],
    byte: usize,
    bit: u32,
    width: u32,
) -> Result<(PhysicalValue, usize), DecodeError> {
    let kind = field.kind();
    let end = add(field, byte, byte_count(bit, width))?;
    if is_string_like(kind) {
        let unit = if kind == DataKind::Unicode2 { 16 } else { 8 };
        if bit != 0 {
            return Err(plan(
                field,
                "bytes or a string that does not start at a byte edge",
            ));
        }
        check_width(field, width.is_multiple_of(unit))?;
        let bytes = message.get(byte..end).ok_or(truncated(field))?;
        return Ok((string_value(field, bytes)?, end));
    }
    let value = match kind {
        DataKind::Uint => {
            check_width(field, (1..=64).contains(&width))?;
            PhysicalValue::Unsigned(extract_bits(field, message, byte, bit, width)?)
        }
        DataKind::Int => {
            check_width(field, (1..=64).contains(&width))?;
            let raw = extract_bits(field, message, byte, bit, width)?;
            let shift = 64 - width;
            // Sign-extend from the field's top bit.
            PhysicalValue::Signed(((raw << shift) as i64) >> shift)
        }
        DataKind::Float => {
            check_width(field, width == 32 || width == 64)?;
            let raw = extract_bits(field, message, byte, bit, width)?;
            PhysicalValue::Float(if width == 32 {
                f64::from(f32::from_bits(raw as u32))
            } else {
                f64::from_bits(raw)
            })
        }
        DataKind::Bcd => {
            check_width(field, width.is_multiple_of(4) && (4..=64).contains(&width))?;
            let raw = extract_bits(field, message, byte, bit, width)?;
            let mut value: u64 = 0;
            for digit_index in (0..width / 4).rev() {
                let digit = (raw >> (digit_index * 4)) & 0xF;
                if digit > 9 {
                    return Err(invalid(field, "a BCD digit above 9"));
                }
                // At most 16 digits, so this stays below 10^16.
                value = value * 10 + digit;
            }
            PhysicalValue::Unsigned(value)
        }
        _ => return Err(plan(field, "an unknown data kind")),
    };
    Ok((value, end))
}

/// A width a numeric type cannot have is a broken plan when it is written in the field, and a
/// broken value when it came from another field of the message.
fn check_width(field: &Field<'_>, ok: bool) -> Result<(), DecodeError> {
    match (ok, field.length_kind()) {
        (true, _) => Ok(()),
        (false, LengthKind::FromField) => Err(invalid(field, "a length its type cannot have")),
        (false, _) => Err(plan(field, "a width its type cannot have")),
    }
}

/// Extracts `width` (1 to 64) bits starting `bit` bits above the least significant bit of the
/// normalized bytes from `byte` on (ISO 22901-1 (2008) 7.3.6.3.2 b to d): the bytes the field
/// touches are read, reversed for a little-endian field, and then shifted and masked.
fn extract_bits(
    field: &Field<'_>,
    message: &[u8],
    byte: usize,
    bit: u32,
    width: u32,
) -> Result<u64, DecodeError> {
    let end = add(field, byte, byte_count(bit, width))?;
    let bytes = message.get(byte..end).ok_or(truncated(field))?;
    // bit <= 7 and width <= 64 touch at most 9 bytes, which fit in a u128.
    let fold = |acc: u128, &b: &u8| (acc << 8) | u128::from(b);
    let normalized = if field.byte_order() == ByteOrder::Little {
        bytes.iter().rev().fold(0, fold)
    } else {
        bytes.iter().fold(0, fold)
    };
    let mask = if width == 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    Ok(((normalized >> bit) as u64) & mask)
}

/// The number of bytes a value of `width` bits touches when it starts at bit position `bit`.
fn byte_count(bit: u32, width: u32) -> usize {
    // At most (7 + 2^32 - 1) / 8 rounded up, which fits in a u32.
    as_usize((u64::from(bit) + u64::from(width)).div_ceil(8) as u32)
}

/// A `FromField` length: the physical value of the field `length_field_id`, in bits, decoded
/// earlier in the same array.
fn width_from_field(field: &Field<'_>, decoded: &[DecodedField]) -> Result<u32, DecodeError> {
    let source = decoded
        .iter()
        .rev()
        .find(|d| d.id == field.length_field_id())
        .ok_or_else(|| plan(field, "a length field that is not decoded before it"))?;
    let bits = match source.value {
        PhysicalValue::Unsigned(n) => Some(n),
        PhysicalValue::Signed(n) => u64::try_from(n).ok(),
        PhysicalValue::Float(f) if f >= 0.0 && f.fract() == 0.0 && f <= u32::MAX as f64 => {
            Some(f as u64)
        }
        PhysicalValue::Float(_) => None,
        PhysicalValue::Text(_) | PhysicalValue::Bytes(_) => {
            return Err(plan(field, "a length field that is not numeric"));
        }
    };
    bits.and_then(|b| u32::try_from(b).ok()).ok_or_else(|| {
        invalid(
            field,
            "a length that is negative, not whole or above 2^32 - 1",
        )
    })
}

/// Extracts a `Terminated` field at a byte edge (ISO 22901-1 (2008) 7.3.6.2, MIN-MAX-LENGTH-TYPE).
fn extract_terminated(
    field: &Field<'_>,
    message: &[u8],
    byte: usize,
) -> Result<(PhysicalValue, usize), DecodeError> {
    let unicode = field.kind() == DataKind::Unicode2;
    let unit = if unicode { 2 } else { 1 };
    let min = as_usize(field.min_bytes());
    let max = as_usize(field.max_bytes());
    if max != 0 && min > max {
        return Err(plan(field, "min_bytes above max_bytes"));
    }
    if unicode && (!min.is_multiple_of(2) || !max.is_multiple_of(2)) {
        return Err(plan(
            field,
            "an odd min_bytes or max_bytes on a Unicode2 field",
        ));
    }
    let rest = message.get(byte..).ok_or(truncated(field))?;
    if rest.len() < min {
        return Err(truncated(field));
    }
    let limit = if max == 0 {
        rest.len()
    } else {
        max.min(rest.len())
    };
    let terminator = match field.termination() {
        Termination::EndOfPdu => None,
        Termination::Zero => Some(0x00),
        Termination::HexFF => Some(0xFF),
        _ => return Err(plan(field, "an unknown termination")),
    };
    // A terminator counts only from min_bytes on, and in a Unicode2 field only as a whole
    // character; it is dropped from the value but consumed.
    let (len, consumed) = terminator
        .and_then(|t| {
            (min..)
                .step_by(unit)
                .take_while(|p| p + unit <= limit)
                .find(|&p| rest[p..p + unit].iter().all(|&b| b == t))
                .map(|p| (p, p + unit))
        })
        .unwrap_or((limit, limit));
    Ok((string_value(field, &rest[..len])?, byte + consumed))
}

/// The internal value of a `Bytes`, `Ascii`, `Utf8` or `Unicode2` field from its bytes.
fn string_value(field: &Field<'_>, bytes: &[u8]) -> Result<PhysicalValue, DecodeError> {
    match field.kind() {
        DataKind::Bytes => Ok(PhysicalValue::Bytes(bytes.to_vec())),
        // Each byte is the code point of the same value: the 8-bit character set ISO 22901-1
        // (2008) 7.3.6.2 gives A_ASCIISTRING.
        DataKind::Ascii => Ok(PhysicalValue::Text(
            bytes.iter().map(|&b| char::from(b)).collect(),
        )),
        DataKind::Utf8 => String::from_utf8(bytes.to_vec())
            .map(PhysicalValue::Text)
            .map_err(|_| invalid(field, "bytes that are not UTF-8")),
        DataKind::Unicode2 => {
            if !bytes.len().is_multiple_of(2) {
                return Err(invalid(field, "a Unicode2 value of an odd number of bytes"));
            }
            let little = field.byte_order() == ByteOrder::Little;
            bytes
                .chunks_exact(2)
                .map(|pair| {
                    let pair = [pair[0], pair[1]];
                    let unit = if little {
                        u16::from_le_bytes(pair)
                    } else {
                        u16::from_be_bytes(pair)
                    };
                    char::from_u32(u32::from(unit))
                        .ok_or_else(|| invalid(field, "a UTF-16 surrogate in a Unicode2 value"))
                })
                .collect::<Result<String, _>>()
                .map(PhysicalValue::Text)
        }
        _ => Err(plan(field, "an unknown data kind")),
    }
}

fn is_string_like(kind: DataKind) -> bool {
    matches!(
        kind,
        DataKind::Bytes | DataKind::Ascii | DataKind::Utf8 | DataKind::Unicode2
    )
}

/// Applies the field's conversion; returns the physical value and the text id.
fn convert(
    field: &Field<'_>,
    internal: PhysicalValue,
) -> Result<(PhysicalValue, u32), DecodeError> {
    let tag = field.compu_type();
    if tag == Compu::NONE || tag == Compu::CompuIdentity {
        return Ok((internal, 0));
    }
    if tag == Compu::CompuBytecode {
        return Err(DecodeError::Unsupported {
            field: field.id(),
            what: "a CompuBytecode conversion",
        });
    }
    if tag == Compu::CompuTextTable {
        let text_id = text_table(field, &internal)?;
        return Ok((internal, text_id));
    }
    let x = match internal {
        PhysicalValue::Unsigned(n) => n as f64,
        PhysicalValue::Signed(n) => n as f64,
        PhysicalValue::Float(f) => f,
        PhysicalValue::Text(_) | PhysicalValue::Bytes(_) => {
            return Err(plan(field, "a numeric conversion on bytes or a string"));
        }
    };
    let result = if let Some(c) = field.compu_as_compu_linear() {
        linear(field, x * c.numerator() + c.offset(), c.denominator())?
    } else if let Some(c) = field.compu_as_compu_scale_linear() {
        // Both limits are inclusive, like ODX's default CLOSED interval (ADR-297); the first
        // matching scale wins.
        let Some(scales) = c.scales().filter(|s| !s.is_empty()) else {
            return Err(plan(field, "a scale-linear conversion without scales"));
        };
        let scale = scales
            .iter()
            .find(|s| s.lower() <= x && x <= s.upper())
            .ok_or_else(|| conversion(field, "no scale contains the value"))?;
        linear(
            field,
            x * scale.numerator() + scale.offset(),
            scale.denominator(),
        )?
    } else if let Some(c) = field.compu_as_compu_table_interp() {
        interpolate(field, c.points(), x)?
    } else if let Some(c) = field.compu_as_compu_rational() {
        let (Some(num), Some(den)) = (c.numerator_coeffs(), c.denominator_coeffs()) else {
            return Err(plan(field, "a rational conversion without coefficients"));
        };
        if num.is_empty() || den.is_empty() {
            return Err(plan(field, "a rational conversion without coefficients"));
        }
        ratio(field, polynomial(num, x), polynomial(den, x))?
    } else {
        return Err(plan(field, "an unknown conversion"));
    };
    Ok((PhysicalValue::Float(result), 0))
}

/// A linear conversion's `numerator / denominator`. Its denominator is a constant, so zero is
/// a broken plan rather than a property of the value.
fn linear(field: &Field<'_>, numerator: f64, denominator: f64) -> Result<f64, DecodeError> {
    if denominator == 0.0 {
        return Err(plan(field, "a linear conversion with a zero denominator"));
    }
    ratio(field, numerator, denominator)
}

/// `numerator / denominator`, failing on a zero denominator or a result that is not finite.
fn ratio(field: &Field<'_>, numerator: f64, denominator: f64) -> Result<f64, DecodeError> {
    if denominator == 0.0 {
        return Err(conversion(field, "the denominator is zero"));
    }
    let result = numerator / denominator;
    if result.is_finite() {
        Ok(result)
    } else {
        Err(conversion(field, "the result is not finite"))
    }
}

/// A polynomial with its coefficients in ascending powers, evaluated by Horner's rule.
fn polynomial(coefficients: Vector<'_, f64>, x: f64) -> f64 {
    coefficients.iter().rev().fold(0.0, |acc, c| acc * x + c)
}

/// Linear interpolation between the two points around `x` (ISO 22901-1 (2008) 7.3.6.6.8). The
/// points' internal values must ascend strictly; a value outside them has no result.
fn interpolate(
    field: &Field<'_>,
    points: Option<Vector<'_, crate::declaration::InterpPoint>>,
    x: f64,
) -> Result<f64, DecodeError> {
    let Some(points) = points.filter(|p| p.len() >= 2) else {
        return Err(plan(
            field,
            "an interpolation table with fewer than two points",
        ));
    };
    // The whole table is checked first, so a broken table fails whatever value arrives.
    // NaN, which is unordered, fails this as well.
    let ascending = points
        .iter()
        .zip(points.iter().skip(1))
        .all(|(a, b)| a.internal().partial_cmp(&b.internal()) == Some(core::cmp::Ordering::Less));
    if !ascending {
        return Err(plan(
            field,
            "interpolation points that do not ascend strictly",
        ));
    }
    for (a, b) in points.iter().zip(points.iter().skip(1)) {
        let (x0, y0, x1, y1) = (a.internal(), a.physical(), b.internal(), b.physical());
        let finite = |result: f64| {
            if result.is_finite() {
                Ok(result)
            } else {
                Err(conversion(field, "the result is not finite"))
            }
        };
        // A value on a point gives that point's physical value exactly.
        if x == x0 {
            return finite(y0);
        }
        if x == x1 {
            return finite(y1);
        }
        if x0 < x && x < x1 {
            // The fraction first, then a weighted sum, as (x - x0) * (y1 - y0) can overflow.
            // Only a span across most of the float range overflows itself; its halves do not,
            // and halving any other span could underflow it to zero.
            let span = x1 - x0;
            let t = if span.is_finite() {
                (x - x0) / span
            } else {
                (x / 2.0 - x0 / 2.0) / (x1 / 2.0 - x0 / 2.0)
            };
            return finite(y0 * (1.0 - t) + y1 * t);
        }
    }
    Err(conversion(
        field,
        "the value is outside the interpolation table",
    ))
}

/// The text id a text table gives an integer internal value: the first entry whose closed
/// range holds it, else the default text; with no default (0) the value has no result
/// (ISO 22901-1 (2008) 7.3.6.6.1).
fn text_table(field: &Field<'_>, internal: &PhysicalValue) -> Result<u32, DecodeError> {
    let x = match *internal {
        PhysicalValue::Unsigned(n) => i128::from(n),
        PhysicalValue::Signed(n) => i128::from(n),
        _ => {
            return Err(plan(
                field,
                "a text table on a value that is not an integer",
            ));
        }
    };
    let Some(table) = field.compu_as_compu_text_table() else {
        return Err(plan(field, "an unknown conversion"));
    };
    let entry = table
        .entries()
        .into_iter()
        .flatten()
        .find(|e| i128::from(e.lower()) <= x && x <= i128::from(e.upper()));
    match (entry, table.default_text_id()) {
        (Some(entry), _) => Ok(entry.text_id()),
        (None, 0) => Err(conversion(
            field,
            "no text entry matches and there is no default",
        )),
        (None, default) => Ok(default),
    }
}

fn as_usize(n: u32) -> usize {
    // diag-ir builds only for targets whose usize has at least 32 bits.
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn as_usize_u64(field: &Field<'_>, n: u64) -> Result<usize, DecodeError> {
    usize::try_from(n).map_err(|_| truncated(field))
}

/// `a + b`; an overflow means the field reaches past any message.
fn add(field: &Field<'_>, a: usize, b: usize) -> Result<usize, DecodeError> {
    a.checked_add(b).ok_or(truncated(field))
}

fn truncated(field: &Field<'_>) -> DecodeError {
    DecodeError::Truncated { field: field.id() }
}

fn invalid(field: &Field<'_>, reason: &'static str) -> DecodeError {
    DecodeError::InvalidValue {
        field: field.id(),
        reason,
    }
}

fn conversion(field: &Field<'_>, reason: &'static str) -> DecodeError {
    DecodeError::Conversion {
        field: field.id(),
        reason,
    }
}

fn plan(field: &Field<'_>, reason: &'static str) -> DecodeError {
    DecodeError::InvalidPlan {
        field: field.id(),
        reason,
    }
}

#[cfg(test)]
#[path = "decode_tests.rs"]
mod tests;

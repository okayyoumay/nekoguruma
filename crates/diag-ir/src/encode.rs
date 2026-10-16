//! Encoding a request's operand from parameter values (ADR-308).
//!
//! [`encode_request`] is the inverse of the extraction [`decode`](crate::decode) does
//! (ADR-297): it writes the internal value of each of a `Request`'s scalar fields at the field's
//! position, after the request's SID and `prefix`, and returns the operand bytes a
//! `ServiceRequest` sends after the SID (`docs/csv-tables.md` 5.3 in `diag-frontend`). Values are
//! internal (coded) values: a field's conversion applies only when a response is decoded.

use std::collections::HashMap;

use thiserror::Error;

use crate::declaration::{
    ByteOrder, DataKind, Field, LengthKind, Placement, Request, Shape, Termination,
};

/// The most bytes one encoded request may take, so a field placed far out cannot make the
/// encoder allocate without bound.
pub const MAX_ENCODED_BYTES: usize = 16 << 20;

/// The internal value given for one request field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamValue<'v> {
    /// For a `Uint`, `Int`, `Bcd` or `Float` field.
    Int(i64),
    /// For a `Float` field.
    Float(f64),
    /// For a `Bytes` field.
    Bytes(&'v [u8]),
    /// For an `Ascii`, `Utf8` or `Unicode2` field.
    Text(&'v str),
}

/// Why a request could not be encoded. Errors about one field carry its `Field.short_name`.
#[derive(Debug, Error, PartialEq)]
pub enum EncodeError {
    /// A scalar field of the request was given no value.
    #[error("no value for field {field}")]
    Missing { field: String },
    /// A value names no scalar field of the request.
    #[error("the request has no field {field}")]
    Unknown { field: String },
    /// The value is not of a kind the field's type takes.
    #[error("field {field}: {reason}")]
    WrongKind { field: String, reason: &'static str },
    /// The value is of the right kind but the field cannot hold it.
    #[error("field {field}: {reason}")]
    OutOfRange { field: String, reason: &'static str },
    /// The field breaks a rule of `ir.fbs` that a frontend must enforce.
    #[error("field {field} is not a valid encode plan: {reason}")]
    InvalidPlan { field: String, reason: &'static str },
    /// The field needs what this encoder does not do: a nested layout, or a length taken from
    /// another field.
    #[error("field {field}: {what} is not supported")]
    Unsupported { field: String, what: &'static str },
    /// The request would take more than [`MAX_ENCODED_BYTES`] bytes.
    #[error("the request is longer than {MAX_ENCODED_BYTES} bytes")]
    TooLarge,
}

/// Encodes `request`'s operand: its `prefix` followed by its fields, each holding the value
/// `values` gives under its `short_name`. Positions count from the SID, so the operand is the
/// message after its first byte, and an `AfterPrevious` first field starts after the prefix.
/// Every scalar field needs a value and every value a field. Bytes no field covers are 0.
pub fn encode_request(
    request: &Request<'_>,
    values: &HashMap<&str, ParamValue<'_>>,
) -> Result<Vec<u8>, EncodeError> {
    let prefix = request.prefix().map_or(&[][..], |p| p.bytes());
    let mut message = Message::default();
    // The SID and the prefix are written first and belong to no field.
    message.bytes.push(request.sid());
    message.bytes.extend_from_slice(prefix);
    message.written = vec![0xFF; message.bytes.len()];
    // Values are given by name, so every field needs a name of its own.
    let mut names = std::collections::HashSet::new();
    for field in request.params().into_iter().flatten() {
        match field.short_name() {
            None | Some("") => return Err(plan(&field, "a field without a name")),
            Some(name) if !names.insert(name) => {
                return Err(plan(&field, "a name another field of the request has"));
            }
            Some(_) => {}
        }
    }
    // A value for no field is reported first, by name in byte order, so the error does not
    // depend on the map's order.
    let mut unknown: Vec<&str> = values
        .keys()
        .copied()
        .filter(|key| !names.contains(key))
        .collect();
    unknown.sort_unstable();
    if let Some(field) = unknown.first() {
        return Err(EncodeError::Unknown {
            field: (*field).to_owned(),
        });
    }
    let mut edge = message.bytes.len();
    // The ends of the fields that run to the end of the message, which nothing may follow.
    let mut open_ends: Vec<(String, usize)> = Vec::new();
    for field in request.params().into_iter().flatten() {
        let name = field.short_name().unwrap_or_default();
        if field.shape_type() != Shape::NONE {
            return Err(unsupported(&field, "a nested layout"));
        }
        let value = values.get(name).ok_or_else(|| EncodeError::Missing {
            field: name.to_owned(),
        })?;
        let (byte, bit) = position(&field, edge)?;
        let end = write(&mut message, &field, *value, byte, bit)?;
        if field.length_kind() == LengthKind::Terminated
            && field.termination() == Termination::EndOfPdu
        {
            open_ends.push((name.to_owned(), end));
        }
        edge = end;
    }
    // A value that runs to the end of the message is read to its end, so nothing may follow it.
    if let Some((field, _)) = open_ends
        .into_iter()
        .find(|(_, end)| *end != message.bytes.len())
    {
        return Err(EncodeError::InvalidPlan {
            field,
            reason: "a value that runs to the end of the message is not last",
        });
    }
    message.bytes.remove(0);
    Ok(message.bytes)
}

/// A request message being written, with the bits each byte has had written, so a field that
/// overlaps another, the SID or the prefix is refused rather than overwriting it.
#[derive(Default)]
struct Message {
    bytes: Vec<u8>,
    written: Vec<u8>,
}

/// The byte and bit position (0 to 7, from the least significant bit) where a field starts.
fn position(field: &Field<'_>, edge: usize) -> Result<(usize, u32), EncodeError> {
    let offset = field.bit_offset();
    match field.placement() {
        Placement::Absolute => Ok((as_usize(offset / 8), offset % 8)),
        Placement::AfterPrevious if offset <= 7 => Ok((edge, offset)),
        Placement::AfterPrevious => Err(plan(field, "an AfterPrevious bit position above 7")),
        _ => Err(plan(field, "an unknown placement")),
    }
}

/// Writes `value` into `message` at the field's position; returns the byte edge after it.
fn write(
    message: &mut Message,
    field: &Field<'_>,
    value: ParamValue<'_>,
    byte: usize,
    bit: u32,
) -> Result<usize, EncodeError> {
    let kind = field.kind();
    if is_string_like(kind) {
        let bytes = string_bytes(field, value)?;
        let unit = if kind == DataKind::Unicode2 { 2 } else { 1 };
        return match field.length_kind() {
            LengthKind::Fixed => {
                let width = field.bit_length();
                if bit != 0 || !width.is_multiple_of(8 * unit as u32) {
                    return Err(plan(
                        field,
                        "bytes or a string that is not whole bytes at a byte edge",
                    ));
                }
                if bytes.len() != as_usize(width / 8) {
                    return Err(range(
                        field,
                        "a value whose length is not the field's fixed length",
                    ));
                }
                put_bytes(message, field, byte, &bytes)
            }
            LengthKind::LeadingLength => {
                let prefix_bits = field.bit_length();
                if !(1..=32).contains(&prefix_bits) {
                    return Err(plan(field, "a leading length prefix outside 1 to 32 bits"));
                }
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if prefix_bits < 64 && len >> prefix_bits != 0 {
                    return Err(range(field, "a value too long for its length prefix"));
                }
                let start = put_bits(message, field, byte, bit, prefix_bits, len)?;
                put_bytes(message, field, start, &bytes)
            }
            LengthKind::Terminated => {
                if bit != 0 {
                    return Err(plan(
                        field,
                        "a terminated value that does not start at a byte edge",
                    ));
                }
                terminated(message, field, byte, &bytes, unit)
            }
            LengthKind::FromField => Err(unsupported(field, "a length taken from another field")),
            _ => Err(plan(field, "an unknown length kind")),
        };
    }
    if field.length_kind() != LengthKind::Fixed {
        return Err(match field.length_kind() {
            LengthKind::FromField => unsupported(field, "a length taken from another field"),
            _ => plan(field, "a leading length or termination on a number"),
        });
    }
    let width = field.bit_length();
    let raw = match kind {
        DataKind::Uint => {
            check_width(field, (1..=64).contains(&width))?;
            let n = int(field, value)?;
            let n = u64::try_from(n).map_err(|_| range(field, "a negative value"))?;
            if width < 64 && n >> width != 0 {
                return Err(range(field, "a value wider than the field"));
            }
            n
        }
        DataKind::Int => {
            check_width(field, (1..=64).contains(&width))?;
            let n = int(field, value)?;
            if width < 64 {
                let half = 1i64 << (width - 1);
                if n < -half || n >= half {
                    return Err(range(field, "a value outside the field's signed range"));
                }
            }
            // Two's complement, cut to the field's width by `put_bits`.
            n as u64
        }
        DataKind::Bcd => {
            check_width(field, width.is_multiple_of(4) && (4..=64).contains(&width))?;
            let n = int(field, value)?;
            let mut n = u64::try_from(n).map_err(|_| range(field, "a negative value"))?;
            let mut raw = 0u64;
            for digit in 0..width / 4 {
                raw |= (n % 10) << (digit * 4);
                n /= 10;
            }
            if n != 0 {
                return Err(range(field, "more digits than the field holds"));
            }
            raw
        }
        DataKind::Float => {
            check_width(field, width == 32 || width == 64)?;
            let x = match value {
                ParamValue::Float(x) => x,
                // An integer is taken when the float holds it exactly.
                // Compared in i128, since `i64::MAX as f64` rounds up to 2^63, which an i64 cast
                // would saturate back to i64::MAX.
                ParamValue::Int(n) if (n as f64) as i128 == i128::from(n) => n as f64,
                ParamValue::Int(_) => {
                    return Err(range(
                        field,
                        "an integer that a float does not hold exactly",
                    ));
                }
                _ => return Err(kind_error(field, "a float field takes a number")),
            };
            if width == 32 {
                let narrow = x as f32;
                if x.is_finite() && f64::from(narrow) != x {
                    return Err(range(field, "a value a 32-bit float does not hold exactly"));
                }
                u64::from(narrow.to_bits())
            } else {
                x.to_bits()
            }
        }
        _ => return Err(plan(field, "an unknown data kind")),
    };
    put_bits(message, field, byte, bit, width, raw)
}

/// The internal integer of a numeric field.
fn int(field: &Field<'_>, value: ParamValue<'_>) -> Result<i64, EncodeError> {
    match value {
        ParamValue::Int(n) => Ok(n),
        _ => Err(kind_error(field, "an integer field takes an integer")),
    }
}

/// The bytes a `Bytes`, `Ascii`, `Utf8` or `Unicode2` field sends for `value`.
fn string_bytes(field: &Field<'_>, value: ParamValue<'_>) -> Result<Vec<u8>, EncodeError> {
    match (field.kind(), value) {
        (DataKind::Bytes, ParamValue::Bytes(b)) => Ok(b.to_vec()),
        // The decoder reads each byte as the code point of the same value (ADR-297).
        (DataKind::Ascii, ParamValue::Text(t)) => t
            .chars()
            .map(|c| {
                u8::try_from(u32::from(c)).map_err(|_| range(field, "a character above U+00FF"))
            })
            .collect(),
        (DataKind::Utf8, ParamValue::Text(t)) => Ok(t.as_bytes().to_vec()),
        (DataKind::Unicode2, ParamValue::Text(t)) => {
            let little = field.byte_order() == ByteOrder::Little;
            let mut out = Vec::with_capacity(t.len() * 2);
            for c in t.chars() {
                // The decoder reads one UTF-16 unit per character and refuses surrogates.
                let unit = u16::try_from(u32::from(c))
                    .map_err(|_| range(field, "a character above U+FFFF"))?;
                out.extend_from_slice(&if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            Ok(out)
        }
        (DataKind::Bytes, _) => Err(kind_error(field, "a bytes field takes bytes")),
        _ => Err(kind_error(field, "a text field takes a string")),
    }
}

/// Writes a terminated value at a byte edge: the bytes and, unless the value fills `max_bytes`
/// or runs to the end of the message, the terminator (ISO 22901-1 (2008) 7.3.6.2).
fn terminated(
    message: &mut Message,
    field: &Field<'_>,
    byte: usize,
    bytes: &[u8],
    unit: usize,
) -> Result<usize, EncodeError> {
    let min = as_usize(field.min_bytes());
    let max = as_usize(field.max_bytes());
    if max != 0 && min > max {
        return Err(plan(field, "min_bytes above max_bytes"));
    }
    if !min.is_multiple_of(unit) || !max.is_multiple_of(unit) {
        return Err(plan(
            field,
            "an odd min_bytes or max_bytes on a Unicode2 field",
        ));
    }
    if bytes.len() < min {
        return Err(range(field, "a value shorter than min_bytes"));
    }
    if max != 0 && bytes.len() > max {
        return Err(range(field, "a value longer than max_bytes"));
    }
    let terminator = match field.termination() {
        Termination::EndOfPdu => None,
        Termination::Zero => Some(0x00),
        Termination::HexFF => Some(0xFF),
        _ => return Err(plan(field, "an unknown termination")),
    };
    let mut end = put_bytes(message, field, byte, bytes)?;
    if let Some(t) = terminator {
        // A terminator inside the value from min_bytes on would end it early when decoded.
        let early = (min..)
            .step_by(unit)
            .take_while(|p| p + unit <= bytes.len())
            .any(|p| bytes[p..p + unit].iter().all(|&b| b == t));
        if early {
            return Err(range(field, "a value that holds its terminator"));
        }
        if max == 0 || bytes.len() + unit <= max {
            end = put_bytes(message, field, end, &vec![t; unit])?;
        }
    }
    Ok(end)
}

/// Copies `bytes` into `message` from byte `at`; returns the byte after them.
fn put_bytes(
    message: &mut Message,
    field: &Field<'_>,
    at: usize,
    bytes: &[u8],
) -> Result<usize, EncodeError> {
    let end = at.checked_add(bytes.len()).ok_or(EncodeError::TooLarge)?;
    grow(message, end)?;
    claim(message, field, at, &vec![0xFF; bytes.len()])?;
    message.bytes[at..end].copy_from_slice(bytes);
    Ok(end)
}

/// Marks the bits `masks` gives for the bytes from `at` on as written; refuses bits written
/// already.
fn claim(
    message: &mut Message,
    field: &Field<'_>,
    at: usize,
    masks: &[u8],
) -> Result<(), EncodeError> {
    let written = &mut message.written[at..at + masks.len()];
    if written.iter().zip(masks).any(|(w, m)| w & m != 0) {
        return Err(plan(
            field,
            "a field that overlaps another field, the SID or the prefix",
        ));
    }
    for (w, m) in written.iter_mut().zip(masks) {
        *w |= m;
    }
    Ok(())
}

/// Writes the low `width` (1 to 64) bits of `raw` starting `bit` bits above the least
/// significant bit of the bytes from `byte` on, normalized as the decoder reads them (reversed
/// for a little-endian field), keeping the other bits of those bytes. Returns the byte after.
fn put_bits(
    message: &mut Message,
    field: &Field<'_>,
    byte: usize,
    bit: u32,
    width: u32,
    raw: u64,
) -> Result<usize, EncodeError> {
    let count = as_usize((u64::from(bit) + u64::from(width)).div_ceil(8) as u32);
    let end = byte.checked_add(count).ok_or(EncodeError::TooLarge)?;
    grow(message, end)?;
    let little = field.byte_order() == ByteOrder::Little;
    let mask = if width == 64 {
        u128::from(u64::MAX)
    } else {
        (1u128 << width) - 1
    };
    // The byte at index i of the window holds bits 8 * from_low(i) up of the normalized value.
    let from_low = |i: usize| if little { i } else { count - 1 - i };
    let masks: Vec<u8> = (0..count)
        .map(|i| ((mask << bit) >> (8 * from_low(i))) as u8)
        .collect();
    claim(message, field, byte, &masks)?;
    let window = &mut message.bytes[byte..end];
    // bit <= 7 and width <= 64 touch at most 9 bytes, which fit in a u128.
    let fold = |acc: u128, &b: &u8| (acc << 8) | u128::from(b);
    let mut normalized = if little {
        window.iter().rev().fold(0, fold)
    } else {
        window.iter().fold(0, fold)
    };
    normalized = (normalized & !(mask << bit)) | ((u128::from(raw) & mask) << bit);
    for (i, slot) in window.iter_mut().enumerate() {
        *slot = (normalized >> (8 * from_low(i))) as u8;
    }
    Ok(end)
}

fn grow(message: &mut Message, end: usize) -> Result<(), EncodeError> {
    if end > MAX_ENCODED_BYTES {
        return Err(EncodeError::TooLarge);
    }
    if message.bytes.len() < end {
        message.bytes.resize(end, 0);
        message.written.resize(end, 0);
    }
    Ok(())
}

fn check_width(field: &Field<'_>, ok: bool) -> Result<(), EncodeError> {
    if ok {
        Ok(())
    } else {
        Err(plan(field, "a width its type cannot have"))
    }
}

fn is_string_like(kind: DataKind) -> bool {
    matches!(
        kind,
        DataKind::Bytes | DataKind::Ascii | DataKind::Utf8 | DataKind::Unicode2
    )
}

fn as_usize(n: u32) -> usize {
    // A u32 fits in a usize on every target the workspace builds for.
    n as usize
}

fn name(field: &Field<'_>) -> String {
    field.short_name().unwrap_or_default().to_owned()
}

fn plan(field: &Field<'_>, reason: &'static str) -> EncodeError {
    EncodeError::InvalidPlan {
        field: name(field),
        reason,
    }
}

fn range(field: &Field<'_>, reason: &'static str) -> EncodeError {
    EncodeError::OutOfRange {
        field: name(field),
        reason,
    }
}

fn kind_error(field: &Field<'_>, reason: &'static str) -> EncodeError {
    EncodeError::WrongKind {
        field: name(field),
        reason,
    }
}

fn unsupported(field: &Field<'_>, what: &'static str) -> EncodeError {
    EncodeError::Unsupported {
        field: name(field),
        what,
    }
}

#[cfg(test)]
#[path = "encode_tests.rs"]
mod tests;

//! Decoding a positive response through its flat fields (design 8.2.2, ADR-297).
//!
//! [`decode_positive_response`] checks a response's SID and prefix, extracts each field of
//! `PositiveResponse.fields` as ISO 22901-1 (2008) 7.3.6.3 describes, and applies the field's
//! conversion (`Compu`). [`decode_dtc_fields`] decodes one of a DTC's field lists the same way,
//! from a byte the caller states. A field with a shape follows its nested layout (ADR-242) and
//! decodes to a [`PhysicalValue::Group`], [`PhysicalValue::Repeat`] or
//! [`PhysicalValue::Select`] (ADR-302). `CompuBytecode` is not evaluated here yet; it fails with
//! [`DecodeError::Unsupported`].

use std::collections::HashMap;

use flatbuffers::{ForwardsUOffset, Vector};
use thiserror::Error;

use crate::declaration::{
    ByteOrder, Compu, DataKind, DtcDefinition, EcuVariant, Field, Layout, LengthKind, Placement,
    PositiveResponse, RepeatCount, Shape, Termination, VariantPattern,
};

/// The nested layouts a document's fields refer to by id: `EcuVariant.layouts`, in id order
/// (the loader checks the order).
pub type Layouts<'a> = Option<Vector<'a, ForwardsUOffset<Layout<'a>>>>;

/// The most nested layouts one message may go through (ADR-242 item 7).
pub const MAX_NESTED_LAYOUTS: usize = 16;

/// The most decoded fields and repeated items one message may give, nested ones included
/// (ADR-242 item 7, ADR-302).
pub const MAX_DECODED_VALUES: usize = 65_536;

/// The most bytes of names, byte strings and text one message may decode to (ADR-302).
/// Absolute placement lets nested fields read the same bytes many times over, so the message's
/// length does not bound this.
pub const MAX_DECODED_BYTES: usize = 16 << 20;

/// The most case, scale, text-table and interpolation entries one message's decoding may look
/// at (ADR-302), so a long table shared by many decoded occurrences cannot take unbounded time.
pub const MAX_TABLE_ENTRIES: usize = 16 << 20;

/// A decoded value. Before a conversion it is the field's internal value; after one, the
/// physical value: a float for every conversion except a text table, which keeps the internal
/// value and reports its text by id ([`DecodedField::text_id`]). A field with a shape decodes
/// to the fields of its nested layout instead (ADR-302).
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
    /// A `Group`: the fields of its one layout occurrence.
    Group(Vec<DecodedField>),
    /// A `Repeat`: the fields of each item, in message order.
    Repeat(Vec<Vec<DecodedField>>),
    /// A `Select`: the case the key chose and the fields of its layout.
    Select {
        /// The index of the chosen case in `Select.cases`; `None` for the default case.
        case: Option<usize>,
        /// The fields of the chosen layout; empty for a case without one.
        fields: Vec<DecodedField>,
    },
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
    /// The field reaches past the end of the message, or of the nested layout or fixed-size
    /// item that holds it.
    #[error("field {field} reaches past the end of the response or of its layout")]
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
    /// The field goes through more than [`MAX_NESTED_LAYOUTS`] nested layouts.
    #[error("field {field} goes through more than {MAX_NESTED_LAYOUTS} nested layouts")]
    TooDeep {
        /// The field whose layout is one too many.
        field: u32,
    },
    /// The message decodes to more than [`MAX_DECODED_VALUES`] fields and items.
    #[error("the response decodes to more than {MAX_DECODED_VALUES} values")]
    TooManyValues,
    /// Decoding the message needs more than [`MAX_DECODED_BYTES`] bytes of names and values, or
    /// looks at more than [`MAX_TABLE_ENTRIES`] table entries.
    #[error("the response is too large to decode")]
    TooLarge,
    /// The field needs what this decoder does not do yet: a `CompuBytecode` conversion.
    #[error("field {field}: {what} is not supported")]
    Unsupported {
        /// The field.
        field: u32,
        /// What the field needs.
        what: &'static str,
    },
}

/// Decodes `message`, a whole positive response starting with its SID, through `response`'s
/// fields, in array order; `layouts` are the document's nested layouts.
///
/// The SID must equal `PositiveResponse.sid` and the bytes after it must start with
/// `PositiveResponse.prefix`. Field positions count from the SID (`ir.fbs`, `Placement`); an
/// `AfterPrevious` first field starts after the prefix. Bytes after the last field are not
/// read and are not an error.
pub fn decode_positive_response(
    response: &PositiveResponse<'_>,
    layouts: Layouts<'_>,
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
        Some(fields) => Decoder::new(layouts, message).top(fields, 1 + prefix.len()),
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

/// Whether `pattern` matches `fields`, the decoded positive response of its service
/// (`VariantPattern.service_id`), as ADR-306 defines it. The field `pattern.field_id` among the
/// response's top-level fields is compared with `pattern.expected` by its decoded value:
///
/// - bytes: `expected` is a prefix of them;
/// - text: `expected` is a prefix of the text's UTF-8 encoding, whatever the field's encoding;
/// - an unsigned integer (a `Uint`, or a `Bcd` as the number its digits spell): `expected`, one
///   to eight bytes read as a big-endian unsigned number, equals it;
/// - a signed integer: `expected`, one to eight bytes read as a big-endian two's complement
///   number, equals it.
///
/// Integers are compared after decoding, so a little-endian field's `expected` is written
/// big-endian like any other. A text table's value is its internal value, not its text. The
/// pattern does not match when `expected` is absent or empty, when the field is missing, when
/// its value is a float or shaped, or when an integer's `expected` is longer than eight bytes.
pub fn pattern_matches(pattern: &VariantPattern<'_>, fields: &[DecodedField]) -> bool {
    let expected = pattern.expected().map_or(&[][..], |e| e.bytes());
    // An empty prefix would match every value, which identifies nothing.
    !expected.is_empty()
        && fields
            .iter()
            .find(|field| field.id == pattern.field_id())
            .is_some_and(|field| value_matches(expected, &field.value))
}

/// Whether `variant` is identified (ADR-315, as ISO 22901-1 (2008) 7.4.3 describes for ECU
/// variant patterns): its `patterns` entries with the same `pattern` number form one pattern,
/// which matches when every entry matches ([`pattern_matches`]), and the variant matches when
/// any of its patterns does. `fields` gives the decoded positive response of an identification
/// service by `Service.id`, or `None` when the service gave none (a negative response, no
/// answer, or a response that did not decode), which no entry of that service matches. A
/// variant without patterns cannot be identified.
pub fn variant_matches<'f>(
    variant: &EcuVariant<'_>,
    mut fields: impl FnMut(u32) -> Option<&'f [DecodedField]>,
) -> bool {
    let Some(entries) = variant.patterns() else {
        return false;
    };
    // Sorted once by pattern, so each pattern is one run of entries.
    let mut entries: Vec<VariantPattern<'_>> = entries.iter().collect();
    entries.sort_by_key(VariantPattern::pattern);
    entries
        .chunk_by(|a, b| a.pattern() == b.pattern())
        .any(|pattern| {
            pattern
                .iter()
                .all(|entry| fields(entry.service_id()).is_some_and(|f| pattern_matches(entry, f)))
        })
}

/// The index in `variants` of the variant selected by its identification responses, or `None`
/// when none matches. As ISO 22901-1 (2008) 7.4.3 lays down, the variants are tried in rising
/// order of their `short_name` (compared byte by byte, whatever order they are given in) and
/// the first that [`variant_matches`] is selected, so every implementation picks the same one.
/// `fields(index, service_id)` gives the decoded positive response of the identification service
/// of the variant at `index`, since each variant's document numbers its own services.
pub fn select_variant<'v, 'f>(
    variants: impl IntoIterator<Item = EcuVariant<'v>>,
    mut fields: impl FnMut(usize, u32) -> Option<&'f [DecodedField]>,
) -> Option<usize> {
    let mut variants: Vec<(usize, EcuVariant<'v>)> = variants.into_iter().enumerate().collect();
    variants.sort_by(|(_, a), (_, b)| a.short_name().cmp(&b.short_name()));
    variants
        .into_iter()
        .find(|(index, variant)| variant_matches(variant, |service| fields(*index, service)))
        .map(|(index, _)| index)
}

fn value_matches(expected: &[u8], value: &PhysicalValue) -> bool {
    match value {
        PhysicalValue::Bytes(bytes) => bytes.starts_with(expected),
        PhysicalValue::Text(text) => text.as_bytes().starts_with(expected),
        PhysicalValue::Unsigned(n) => big_endian(expected) == Some(*n),
        PhysicalValue::Signed(n) => big_endian(expected).is_some_and(|raw| {
            // Sign-extend from the width `expected` was written in.
            let shift = 64 - 8 * expected.len() as u32;
            ((raw << shift) as i64) >> shift == *n
        }),
        PhysicalValue::Float(_)
        | PhysicalValue::Group(_)
        | PhysicalValue::Repeat(_)
        | PhysicalValue::Select { .. } => false,
    }
}

/// `bytes` as a big-endian number; `None` unless it has one to eight bytes.
fn big_endian(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || bytes.len() > 8 {
        return None;
    }
    Some(bytes.iter().fold(0, |n, &b| (n << 8) | u64::from(b)))
}

/// Decodes one of `dtc`'s field lists from `message`, with the list's layout starting at byte
/// `start` of it; `layouts` are the document's nested layouts.
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
    layouts: Layouts<'_>,
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
    Decoder::new(layouts, rest).top(fields, 0)
}

type Fields<'a> = Vector<'a, ForwardsUOffset<Field<'a>>>;

/// What a later field may need of a decoded one: a length or count (its physical value, when
/// numeric) and a key or marker (its internal value as an integer, ADR-242 item 5).
#[derive(Clone, Copy)]
struct Seen {
    /// The physical value, when it is a number.
    number: Option<Number>,
    /// The internal value as a key: an integer, or bytes or a string of up to 8 bytes read as
    /// a big-endian integer and stored bit for bit in an `i64`.
    key: Option<Key>,
}

#[derive(Clone, Copy)]
enum Number {
    Unsigned(u64),
    Signed(i64),
    Float(f64),
}

/// The state of decoding one message: its bytes, the layouts, the fields decoded so far that
/// later ones may refer to, and the limits.
struct Decoder<'a, 'm> {
    layouts: Layouts<'a>,
    message: &'m [u8],
    /// The fields of each layout occurrence being decoded, by id, innermost last; a later
    /// occurrence of an id replaces an earlier one, so a reference finds the latest.
    scopes: Vec<HashMap<u32, Seen>>,
    /// Decoded fields and items so far, nested ones included.
    values: usize,
    /// Bytes of names and values decoded so far.
    bytes: usize,
    /// Table entries looked at so far.
    entries: usize,
}

impl<'a, 'm> Decoder<'a, 'm> {
    fn new(layouts: Layouts<'a>, message: &'m [u8]) -> Self {
        Decoder {
            layouts,
            message,
            scopes: Vec::new(),
            values: 0,
            bytes: 0,
            entries: 0,
        }
    }

    /// Decodes a top-level field array, whose positions count from byte 0 and whose first
    /// `AfterPrevious` field starts at `first`.
    fn top(mut self, fields: Fields<'_>, first: usize) -> Result<Vec<DecodedField>, DecodeError> {
        let end = self.message.len();
        self.layout(fields, 0, first, end)
            .map(|(decoded, _)| decoded)
    }

    /// Decodes one layout occurrence whose fields' positions count from byte `base`, whose first
    /// `AfterPrevious` field starts at `first`, and whose fields must end by byte `end`. Returns
    /// the fields and the byte edge after the one that ends last.
    fn layout(
        &mut self,
        fields: Fields<'_>,
        base: usize,
        first: usize,
        end: usize,
    ) -> Result<(Vec<DecodedField>, usize), DecodeError> {
        self.scopes.push(HashMap::new());
        let result = self.fields(fields, base, first, end);
        self.scopes.pop();
        result
    }

    fn fields(
        &mut self,
        fields: Fields<'_>,
        base: usize,
        first: usize,
        end: usize,
    ) -> Result<(Vec<DecodedField>, usize), DecodeError> {
        let window = &self.message[..end];
        // Not reserved from fields.len(): the first field may fail at once.
        let mut decoded: Vec<DecodedField> = Vec::new();
        // The byte edge after the previous field, and after the field that ends last.
        let mut edge = first;
        let mut extent = first;
        for field in fields {
            let id = field.id();
            let (byte, bit) = position(&field, base, edge)?;
            let (value, text_id, after) = if field.shape_type() == Shape::NONE {
                let (internal, after) = extract(self, &field, window, byte, bit)?;
                let key = key_of(&field, &internal);
                let mut visited = 0;
                let converted = convert(&field, internal, &mut visited);
                self.scan(visited)?;
                let (value, text_id) = converted?;
                self.remember(
                    id,
                    Seen {
                        number: number_of(&value),
                        key,
                    },
                );
                (value, text_id, after)
            } else {
                if bit != 0 {
                    return Err(plan(
                        &field,
                        "a nested layout that does not start at a byte edge",
                    ));
                }
                // Even a shaped field that consumes nothing must start within its layout.
                if byte > end {
                    return Err(truncated(&field));
                }
                let (value, after) = self.shaped(&field, byte, end)?;
                (value, 0, after)
            };
            edge = after;
            extent = extent.max(after);
            let name = field.short_name().unwrap_or_default();
            let value_bytes = match &value {
                PhysicalValue::Bytes(b) => b.len(),
                PhysicalValue::Text(t) => t.len(),
                _ => 0,
            };
            self.count_value(name.len().saturating_add(value_bytes))?;
            decoded.push(DecodedField {
                id,
                short_name: name.to_owned(),
                value,
                text_id,
                unit_id: field.unit_id(),
            });
        }
        Ok((decoded, extent))
    }

    /// Counts one decoded field or item of `bytes` bytes against the limits.
    fn count_value(&mut self, bytes: usize) -> Result<(), DecodeError> {
        self.values += 1;
        if self.values > MAX_DECODED_VALUES {
            return Err(DecodeError::TooManyValues);
        }
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > MAX_DECODED_BYTES {
            return Err(DecodeError::TooLarge);
        }
        Ok(())
    }

    /// Counts `entries` table entries looked at against the limit.
    fn scan(&mut self, entries: usize) -> Result<(), DecodeError> {
        self.entries = self.entries.saturating_add(entries);
        if self.entries > MAX_TABLE_ENTRIES {
            return Err(DecodeError::TooLarge);
        }
        Ok(())
    }

    fn remember(&mut self, id: u32, seen: Seen) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(id, seen);
        }
    }

    /// The latest decoded field `id` in this layout occurrence or an enclosing one.
    fn seen(&self, id: u32) -> Option<Seen> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(&id).copied())
    }

    /// Decodes a field with a shape starting at byte `start`, with the enclosing layout ending
    /// at byte `end`. Returns the value and the byte edge after the field.
    fn shaped(
        &mut self,
        field: &Field<'_>,
        start: usize,
        end: usize,
    ) -> Result<(PhysicalValue, usize), DecodeError> {
        if let Some(group) = field.shape_as_group() {
            let layout = self.find(field, group.layout_id())?;
            let (fields, after) = self.nested(field, &layout, start, end)?;
            Ok((PhysicalValue::Group(fields), after))
        } else if let Some(repeat) = field.shape_as_repeat() {
            let layout = self.find(field, repeat.layout_id())?;
            let mut items = Vec::new();
            let mut at = start;
            match repeat.count() {
                RepeatCount::Fixed => {
                    let size = as_usize(repeat.item_byte_size());
                    if size == 0 {
                        return Err(plan(field, "a fixed repetition with an item size of 0"));
                    }
                    for _ in 0..repeat.fixed_count() {
                        let item_end = add(field, at, size)?;
                        if item_end > end {
                            return Err(truncated(field));
                        }
                        let (fields, _) = self.nested(field, &layout, at, item_end)?;
                        self.count_value(0)?;
                        items.push(fields);
                        at = item_end;
                    }
                }
                RepeatCount::FromField => {
                    let count = self.count(field, repeat.count_field_id())?;
                    for _ in 0..count {
                        let (fields, after) = self.nested(field, &layout, at, end)?;
                        if after == at {
                            break;
                        }
                        self.count_value(0)?;
                        items.push(fields);
                        at = after;
                    }
                }
                RepeatCount::UntilMarker => {
                    let Some(marker) = repeat.marker() else {
                        return Err(plan(field, "an end-marker repetition without a marker"));
                    };
                    if marker.shape_type() != Shape::NONE
                        || marker.length_kind() != LengthKind::Fixed
                        || marker.placement() != Placement::Absolute
                        || marker.bit_offset() != 0
                    {
                        return Err(plan(
                            field,
                            "a marker that is not a fixed-length field at offset 0",
                        ));
                    }
                    // A key is at most 64 bits (ADR-242 item 5); checked before any marker is
                    // read, so a wide marker cannot be copied at every item.
                    if marker.bit_length() > 64 {
                        return Err(plan(field, "a marker wider than 64 bits"));
                    }
                    let (lower, upper) = (repeat.marker_lower(), repeat.marker_upper());
                    while at < end {
                        let window = &self.message[..end];
                        match extract(self, &marker, window, at, 0) {
                            Ok((internal, _)) => {
                                let Some(key) = key_of(&marker, &internal) else {
                                    return Err(plan(field, "a marker that is not an integer"));
                                };
                                if key.within(lower, upper) {
                                    break;
                                }
                            }
                            // Too few bytes left for a marker, or bytes that are no value of the
                            // marker's type: no marker, so an item follows.
                            Err(
                                DecodeError::Truncated { .. } | DecodeError::InvalidValue { .. },
                            ) => {}
                            Err(error) => return Err(error),
                        }
                        let (fields, after) = self.nested(field, &layout, at, end)?;
                        if after == at {
                            break;
                        }
                        self.count_value(0)?;
                        items.push(fields);
                        at = after;
                    }
                }
                RepeatCount::UntilEndOfPdu => {
                    while at < end {
                        let (fields, after) = self.nested(field, &layout, at, end)?;
                        if after == at {
                            break;
                        }
                        self.count_value(0)?;
                        items.push(fields);
                        at = after;
                    }
                }
                _ => return Err(plan(field, "an unknown repetition count")),
            }
            Ok((PhysicalValue::Repeat(items), at))
        } else if let Some(select) = field.shape_as_select() {
            let key = self
                .seen(select.key_field_id())
                .ok_or_else(|| plan(field, "a key field that is not decoded before it"))?
                .key
                .ok_or_else(|| plan(field, "a key that is not an integer or up to 8 bytes"))?;
            let found = select.cases().map(|cases| {
                let at = cases.iter().position(|c| key.within(c.lower(), c.upper()));
                // The cases looked at: up to the match, or all of them.
                (
                    at.map_or(cases.len(), |at| at + 1),
                    at.map(|at| (Some(at), cases.get(at).layout_id())),
                )
            });
            self.scan(found.map_or(0, |(looked, _)| looked))?;
            let chosen = found.and_then(|(_, chosen)| chosen);
            let (case, layout_id) = match chosen {
                Some(chosen) => chosen,
                None if select.has_default() => (None, select.default_layout_id()),
                None => return Err(invalid(field, "a key that no case matches")),
            };
            if layout_id == 0 {
                return Ok((
                    PhysicalValue::Select {
                        case,
                        fields: Vec::new(),
                    },
                    start,
                ));
            }
            let layout = self.find(field, layout_id)?;
            let (fields, after) = self.nested(field, &layout, start, end)?;
            Ok((PhysicalValue::Select { case, fields }, after))
        } else {
            Err(plan(field, "an unknown shape"))
        }
    }

    /// The layout `id` refers to.
    fn find(&self, field: &Field<'_>, id: u32) -> Result<Layout<'a>, DecodeError> {
        self.layouts
            .and_then(|layouts| {
                layouts.lookup_by_key(id, |layout, id| layout.key_compare_with_value(*id))
            })
            .ok_or_else(|| plan(field, "a layout id that names no layout"))
    }

    /// Decodes one occurrence of `layout` starting at byte `start`, inside an enclosing layout
    /// or item that ends at byte `end`. A layout with a `byte_size` ends there and its fields
    /// must end within it; otherwise it ends after the field that ends last.
    fn nested(
        &mut self,
        field: &Field<'_>,
        layout: &Layout<'_>,
        start: usize,
        end: usize,
    ) -> Result<(Vec<DecodedField>, usize), DecodeError> {
        // The top level is one scope; each nested layout adds one.
        if self.scopes.len() > MAX_NESTED_LAYOUTS {
            return Err(DecodeError::TooDeep { field: field.id() });
        }
        let size = as_usize(layout.byte_size());
        let bound = if size == 0 {
            end
        } else {
            let bound = add(field, start, size)?;
            if bound > end {
                return Err(truncated(field));
            }
            bound
        };
        let Some(fields) = layout.fields() else {
            return Ok((Vec::new(), if size == 0 { start } else { bound }));
        };
        let (decoded, extent) = self.layout(fields, start, start, bound)?;
        Ok((decoded, if size == 0 { extent } else { bound }))
    }

    /// A `FromField` repetition's item count: the physical value of `count_field_id`.
    fn count(&self, field: &Field<'_>, id: u32) -> Result<u64, DecodeError> {
        let seen = self
            .seen(id)
            .ok_or_else(|| plan(field, "a count field that is not decoded before it"))?;
        whole(field, seen, "a count that is negative or not whole")
    }
}

/// The physical value of a referenced field as a whole, non-negative number.
fn whole(field: &Field<'_>, seen: Seen, reason: &'static str) -> Result<u64, DecodeError> {
    match seen.number {
        Some(Number::Unsigned(n)) => Ok(n),
        Some(Number::Signed(n)) => u64::try_from(n).map_err(|_| invalid(field, reason)),
        Some(Number::Float(f)) if f >= 0.0 && f.fract() == 0.0 && f < u64::MAX as f64 => {
            Ok(f as u64)
        }
        Some(Number::Float(_)) => Err(invalid(field, reason)),
        None => Err(plan(field, "a referenced field that is not numeric")),
    }
}

fn number_of(value: &PhysicalValue) -> Option<Number> {
    match *value {
        PhysicalValue::Unsigned(n) => Some(Number::Unsigned(n)),
        PhysicalValue::Signed(n) => Some(Number::Signed(n)),
        PhysicalValue::Float(f) => Some(Number::Float(f)),
        _ => None,
    }
}

/// A field's internal value as a key (ADR-242 item 5). A signed integer is compared as signed;
/// an unsigned integer, and bytes or a string of up to 8 bytes read as a big-endian integer,
/// are compared as unsigned, with the case or marker limits (stored bit for bit in `int64`) read
/// as unsigned too. A float, or a longer byte string, is no key.
fn key_of(field: &Field<'_>, internal: &PhysicalValue) -> Option<Key> {
    // At most 9 bytes are rebuilt: enough to tell that a value is too long to be a key.
    let bytes: Vec<u8> = match internal {
        PhysicalValue::Unsigned(n) => return Some(Key::Unsigned(*n)),
        PhysicalValue::Signed(n) => return Some(Key::Signed(*n)),
        PhysicalValue::Bytes(b) => b.iter().copied().take(9).collect(),
        // Each character is one byte of the message (`string_value`).
        PhysicalValue::Text(t) if field.kind() == DataKind::Ascii => {
            t.chars().take(9).map(|c| c as u8).collect()
        }
        PhysicalValue::Text(t) if field.kind() == DataKind::Utf8 => {
            t.as_bytes().iter().copied().take(9).collect()
        }
        PhysicalValue::Text(t) if field.kind() == DataKind::Unicode2 => {
            let little = field.byte_order() == ByteOrder::Little;
            t.encode_utf16()
                .take(5)
                .flat_map(|u| {
                    if little {
                        u.to_le_bytes()
                    } else {
                        u.to_be_bytes()
                    }
                })
                .collect()
        }
        _ => return None,
    };
    if bytes.len() > 8 {
        return None;
    }
    Some(Key::Unsigned(
        bytes.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)),
    ))
}

/// A key or marker value, compared with the inclusive limits `lower..=upper` of `ir.fbs`.
#[derive(Clone, Copy)]
enum Key {
    Signed(i64),
    Unsigned(u64),
}

impl Key {
    fn within(self, lower: i64, upper: i64) -> bool {
        match self {
            Key::Signed(k) => lower <= k && k <= upper,
            Key::Unsigned(k) => (lower as u64) <= k && k <= (upper as u64),
        }
    }
}

/// The byte and the bit position (0 to 7, from the least significant bit) where a field starts,
/// in a layout that starts at byte `base`.
fn position(field: &Field<'_>, base: usize, edge: usize) -> Result<(usize, u32), DecodeError> {
    let offset = field.bit_offset();
    match field.placement() {
        Placement::Absolute => Ok((add(field, base, as_usize(offset / 8))?, offset % 8)),
        Placement::AfterPrevious if offset <= 7 => Ok((edge, offset)),
        Placement::AfterPrevious => Err(plan(field, "an AfterPrevious bit position above 7")),
        _ => Err(plan(field, "an unknown placement")),
    }
}

/// Extracts a field's internal value; returns it with the byte edge after the field.
fn extract(
    decoder: &Decoder<'_, '_>,
    field: &Field<'_>,
    message: &[u8],
    byte: usize,
    bit: u32,
) -> Result<(PhysicalValue, usize), DecodeError> {
    let kind = field.kind();
    let string_like = is_string_like(kind);
    match field.length_kind() {
        LengthKind::Fixed => extract_with_width(field, message, byte, bit, field.bit_length()),
        LengthKind::FromField => {
            let width = width_from_field(field, decoder)?;
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
/// earlier in the same layout occurrence or an enclosing one.
fn width_from_field(field: &Field<'_>, decoder: &Decoder<'_, '_>) -> Result<u32, DecodeError> {
    let seen = decoder
        .seen(field.length_field_id())
        .ok_or_else(|| plan(field, "a length field that is not decoded before it"))?;
    if seen.number.is_none() {
        return Err(plan(field, "a length field that is not numeric"));
    }
    let reason = "a length that is negative, not whole or above 2^32 - 1";
    whole(field, seen, reason)
        .ok()
        .and_then(|bits| u32::try_from(bits).ok())
        .ok_or_else(|| invalid(field, reason))
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

/// What a scalar field decodes to, by [`decode_positive_response`]'s rules: the value
/// `Op::DecodeField` gives a procedure (ADR-316).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarKind {
    /// An unsigned, signed or BCD number, also under a text table, which keeps the number.
    Integer,
    /// A float, or the result of a numeric conversion.
    Float,
    /// A string.
    Text,
    /// Bytes.
    Bytes,
}

/// What `field` decodes to, or why it does not decode to a scalar: a nested layout, a
/// conversion the decoder does not run, or a numeric conversion of a string. Decided from the
/// declaration alone, as [`convert`] decides it, so a caller can type a field before any
/// response arrives.
pub fn scalar_kind(field: &Field<'_>) -> Result<ScalarKind, &'static str> {
    if field.shape_type() != Shape::NONE {
        return Err("a field with a nested layout");
    }
    let tag = field.compu_type();
    if tag == Compu::CompuBytecode {
        return Err("a CompuBytecode conversion");
    }
    let by_kind = match field.kind() {
        DataKind::Uint | DataKind::Int | DataKind::Bcd => ScalarKind::Integer,
        DataKind::Float => ScalarKind::Float,
        DataKind::Ascii | DataKind::Utf8 | DataKind::Unicode2 => ScalarKind::Text,
        DataKind::Bytes => ScalarKind::Bytes,
        _ => return Err("an unknown data kind"),
    };
    if tag == Compu::NONE || tag == Compu::CompuIdentity || tag == Compu::CompuTextTable {
        return Ok(by_kind);
    }
    if is_string_like(field.kind()) {
        return Err("a numeric conversion on bytes or a string");
    }
    Ok(ScalarKind::Float)
}

fn is_string_like(kind: DataKind) -> bool {
    matches!(
        kind,
        DataKind::Bytes | DataKind::Ascii | DataKind::Utf8 | DataKind::Unicode2
    )
}

/// Applies the field's conversion; returns the physical value and the text id, and adds the
/// table entries it looked at (scales and text entries up to the first match, all points and
/// coefficients) to `visited`.
fn convert(
    field: &Field<'_>,
    internal: PhysicalValue,
    visited: &mut usize,
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
        let text_id = text_table(field, &internal, visited)?;
        return Ok((internal, text_id));
    }
    let x = match internal {
        PhysicalValue::Unsigned(n) => n as f64,
        PhysicalValue::Signed(n) => n as f64,
        PhysicalValue::Float(f) => f,
        _ => return Err(plan(field, "a numeric conversion on bytes or a string")),
    };
    let result = if let Some(c) = field.compu_as_compu_linear() {
        linear(field, x * c.numerator() + c.offset(), c.denominator())?
    } else if let Some(c) = field.compu_as_compu_scale_linear() {
        // Both limits are inclusive, like ODX's default CLOSED interval (ADR-297); the first
        // matching scale wins.
        let Some(scales) = c.scales().filter(|s| !s.is_empty()) else {
            return Err(plan(field, "a scale-linear conversion without scales"));
        };
        let at = scales.iter().position(|s| s.lower() <= x && x <= s.upper());
        *visited += at.map_or(scales.len(), |at| at + 1);
        let scale = scales.get(at.ok_or_else(|| conversion(field, "no scale contains the value"))?);
        linear(
            field,
            x * scale.numerator() + scale.offset(),
            scale.denominator(),
        )?
    } else if let Some(c) = field.compu_as_compu_table_interp() {
        *visited += c.points().map_or(0, |p| p.len());
        interpolate(field, c.points(), x)?
    } else if let Some(c) = field.compu_as_compu_rational() {
        let (Some(num), Some(den)) = (c.numerator_coeffs(), c.denominator_coeffs()) else {
            return Err(plan(field, "a rational conversion without coefficients"));
        };
        if num.is_empty() || den.is_empty() {
            return Err(plan(field, "a rational conversion without coefficients"));
        }
        *visited += num.len() + den.len();
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
fn text_table(
    field: &Field<'_>,
    internal: &PhysicalValue,
    visited: &mut usize,
) -> Result<u32, DecodeError> {
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
    let entries = table.entries();
    let at = entries
        .into_iter()
        .flatten()
        .position(|e| i128::from(e.lower()) <= x && x <= i128::from(e.upper()));
    *visited += at.map_or(entries.map_or(0, |e| e.len()), |at| at + 1);
    let entry = at.zip(entries).map(|(at, entries)| entries.get(at));
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

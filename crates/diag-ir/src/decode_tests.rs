use super::*;
use crate::IR_SCHEMA_VERSION;
use crate::declaration::{
    CompuBytecodeT, CompuLinearT, CompuRationalT, CompuScaleLinearT, CompuT, CompuTableInterpT,
    CompuTextTableT, DtcDefinitionT, EcuDocumentT, EcuVariantT, FieldT, GroupT, InterpPointT,
    LayoutT, LinearScaleT, PositiveResponseT, RepeatT, SelectCaseT, SelectT, ServiceT, ShapeT,
    TextEntryT, encode_ecu_document, load_ecu_document,
};

const SID: u8 = 0x62;

/// Decodes `message` through a positive response with SID 0x62, `prefix` and `fields`, after
/// encoding it in a document and loading that through the checking loader.
fn decode(
    prefix: &[u8],
    fields: Vec<FieldT>,
    message: &[u8],
) -> Result<Vec<DecodedField>, DecodeError> {
    decode_nested(prefix, fields, vec![], message)
}

/// As [`decode`], with `layouts` as the variant's nested layouts.
fn decode_nested(
    prefix: &[u8],
    fields: Vec<FieldT>,
    layouts: Vec<LayoutT>,
    message: &[u8],
) -> Result<Vec<DecodedField>, DecodeError> {
    let service = ServiceT {
        id: 1,
        short_name: Some("s".into()),
        positive: Some(Box::new(PositiveResponseT {
            sid: SID,
            prefix: Some(prefix.to_vec()),
            fields: Some(fields),
        })),
        ..Default::default()
    };
    let document = EcuDocumentT {
        schema_version: IR_SCHEMA_VERSION,
        source_format: Some("csv-starlark".into()),
        source_digest: Some("00".into()),
        variant: Some(Box::new(EcuVariantT {
            id: 1,
            services: Some(vec![service]),
            layouts: Some(layouts),
            ..Default::default()
        })),
        procedure: None,
    };
    let buf = encode_ecu_document(&document);
    let document = load_ecu_document(&buf).expect("test document loads");
    let variant = document.variant().expect("test document has a variant");
    let positive = variant
        .services()
        .map(|s| s.get(0))
        .and_then(|s| s.positive())
        .expect("test document has a positive response");
    decode_positive_response(&positive, variant.layouts(), message)
}

fn values(result: Result<Vec<DecodedField>, DecodeError>) -> Vec<PhysicalValue> {
    result
        .expect("decodes")
        .into_iter()
        .map(|d| d.value)
        .collect()
}

/// A `Fixed` field at an absolute bit offset.
fn at(id: u32, kind: DataKind, bit_offset: u32, bit_length: u32) -> FieldT {
    FieldT {
        id,
        short_name: Some(format!("f{id}")),
        bit_offset,
        bit_length,
        kind,
        ..Default::default()
    }
}

/// A `Fixed` field that follows the previous one.
fn next(id: u32, kind: DataKind, bit_length: u32) -> FieldT {
    FieldT {
        placement: Placement::AfterPrevious,
        ..at(id, kind, 0, bit_length)
    }
}

fn with_compu(field: FieldT, compu: CompuT) -> FieldT {
    FieldT { compu, ..field }
}

fn little(field: FieldT) -> FieldT {
    FieldT {
        byte_order: ByteOrder::Little,
        ..field
    }
}

use PhysicalValue::{Bytes, Float, Signed, Text, Unsigned};

// ---------------------------------------------------------------- SID and prefix

#[test]
fn the_vin_is_decoded_after_the_echoed_identifier() {
    let mut message = vec![SID, 0xF1, 0x90];
    message.extend_from_slice(b"WVWZZZ1JZXW000001");
    let decoded = decode(
        &[0xF1, 0x90],
        vec![at(1, DataKind::Ascii, 24, 136)],
        &message,
    )
    .expect("decodes");
    assert_eq!(
        decoded,
        vec![DecodedField {
            id: 1,
            short_name: "f1".into(),
            value: Text("WVWZZZ1JZXW000001".into()),
            text_id: 0,
            unit_id: 0,
        }]
    );
}

#[test]
fn an_empty_response_is_rejected() {
    assert_eq!(decode(&[], vec![], &[]), Err(DecodeError::Empty));
}

#[test]
fn another_sid_is_rejected() {
    assert_eq!(
        decode(&[], vec![], &[0x63]),
        Err(DecodeError::WrongSid {
            expected: SID,
            found: 0x63
        })
    );
}

#[test]
fn a_prefix_that_does_not_match_is_rejected() {
    let fields = || vec![next(1, DataKind::Uint, 8)];
    assert_eq!(
        decode(&[0xF1, 0x90], fields(), &[SID, 0xF1, 0x91, 0x00]),
        Err(DecodeError::PrefixMismatch)
    );
    // A response that ends inside the prefix does not start with it either.
    assert_eq!(
        decode(&[0xF1, 0x90], fields(), &[SID, 0xF1]),
        Err(DecodeError::PrefixMismatch)
    );
}

#[test]
fn bytes_after_the_last_field_are_not_read() {
    assert_eq!(
        values(decode(
            &[],
            vec![next(1, DataKind::Uint, 8)],
            &[SID, 7, 8, 9]
        )),
        vec![Unsigned(7)]
    );
}

// ---------------------------------------------------------------- Positions and widths

#[test]
fn a_bit_field_is_extracted_across_a_byte_edge() {
    // 4 bits at bit position 5 of byte 1 touch bytes 1 and 2 (ISO 22901-1 (2008) 7.3.6.3).
    // Big endian: 0b0000_0001_1010_0000 >> 5 = 0b1101, masked to 4 bits = 0xD.
    let field = || at(1, DataKind::Uint, 8 + 5, 4);
    assert_eq!(
        values(decode(&[], vec![field()], &[SID, 0x01, 0xA0])),
        vec![Unsigned(0xD)]
    );
    // Little endian reverses the two bytes first: 0xA001 >> 5 = 0x500, masked = 0x0.
    assert_eq!(
        values(decode(&[], vec![little(field())], &[SID, 0x01, 0xA0])),
        vec![Unsigned(0x0)]
    );
    assert_eq!(
        values(decode(&[], vec![little(field())], &[SID, 0xE0, 0x01])),
        vec![Unsigned(0xF)]
    );
}

#[test]
fn integers_of_every_width_are_read() {
    let message = [
        SID, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE, 0x80, 0x01,
    ];
    let fields = vec![
        at(1, DataKind::Uint, 8, 64),
        at(2, DataKind::Int, 8, 64),
        // 12 bits from bit 0 of bytes 8 and 9 (0xFE80 & 0xFFF = 0xE80): negative.
        at(3, DataKind::Int, 64, 12),
        at(4, DataKind::Uint, 64, 12),
        at(5, DataKind::Int, 80, 1),
        little(at(6, DataKind::Uint, 72, 16)),
    ];
    assert_eq!(
        values(decode(&[], fields, &message)),
        vec![
            Unsigned(0xFFFF_FFFF_FFFF_FFFE),
            Signed(-2),
            Signed(0xE80 - 0x1000),
            Unsigned(0xE80),
            Signed(-1),
            Unsigned(0x0180),
        ]
    );
}

#[test]
fn floats_are_read_from_their_bit_patterns_in_either_byte_order() {
    let mut message = vec![SID];
    message.extend_from_slice(&1.5f32.to_be_bytes());
    message.extend_from_slice(&(-0.25f64).to_le_bytes());
    let fields = vec![
        next(1, DataKind::Float, 32),
        little(next(2, DataKind::Float, 64)),
    ];
    assert_eq!(
        values(decode(&[], fields, &message)),
        vec![Float(1.5), Float(-0.25)]
    );
}

#[test]
fn bcd_digits_spell_a_number() {
    let fields = vec![
        next(1, DataKind::Bcd, 16),
        little(next(2, DataKind::Bcd, 16)),
        at(3, DataKind::Bcd, 8 + 4, 4),
    ];
    assert_eq!(
        values(decode(&[], fields, &[SID, 0x12, 0x34, 0x78, 0x56])),
        vec![Unsigned(1234), Unsigned(5678), Unsigned(1)]
    );
    assert_eq!(
        decode(&[], vec![next(1, DataKind::Bcd, 8)], &[SID, 0x1A]),
        Err(DecodeError::InvalidValue {
            field: 1,
            reason: "a BCD digit above 9"
        })
    );
}

#[test]
fn after_previous_starts_at_the_byte_edge_after_the_previous_field() {
    let fields = vec![
        // After the prefix.
        next(1, DataKind::Uint, 8),
        // A 3-bit field leaves the rest of its byte unused.
        at(2, DataKind::Uint, 3 * 8 + 2, 3),
        // So the next field starts at byte 4, at bit position 4.
        FieldT {
            bit_offset: 4,
            ..next(3, DataKind::Uint, 4)
        },
        next(4, DataKind::Uint, 8),
    ];
    let message = [SID, 0xAA, 0x01, 0b0001_0100, 0b1010_0000, 0x5C];
    assert_eq!(
        values(decode(&[0xAA], fields, &message)),
        vec![
            Unsigned(1),
            Unsigned(0b101),
            Unsigned(0b1010),
            Unsigned(0x5C)
        ]
    );
}

#[test]
fn a_field_past_the_end_is_truncated() {
    assert_eq!(
        decode(&[], vec![next(1, DataKind::Uint, 16)], &[SID, 0x01]),
        Err(DecodeError::Truncated { field: 1 })
    );
    assert_eq!(
        decode(&[], vec![at(1, DataKind::Bytes, u32::MAX - 7, 8)], &[SID]),
        Err(DecodeError::Truncated { field: 1 })
    );
}

// ---------------------------------------------------------------- Length kinds

#[test]
fn a_length_from_another_field_is_in_bits() {
    let length = |id, kind, field| FieldT {
        length_kind: LengthKind::FromField,
        length_field_id: field,
        ..next(id, kind, 0)
    };
    let fields = vec![
        next(1, DataKind::Uint, 8),
        length(2, DataKind::Bytes, 1),
        next(3, DataKind::Uint, 8),
        length(4, DataKind::Uint, 3),
    ];
    let message = [SID, 24, 0xA, 0xB, 0xC, 12, 0x0F, 0xFF];
    assert_eq!(
        values(decode(&[], fields, &message)),
        vec![
            Unsigned(24),
            Bytes(vec![0xA, 0xB, 0xC]),
            Unsigned(12),
            Unsigned(0xFFF),
        ]
    );
}

#[test]
fn a_length_field_must_hold_a_valid_width() {
    let fields = |source: FieldT, kind| {
        vec![
            source,
            FieldT {
                length_kind: LengthKind::FromField,
                length_field_id: 1,
                ..next(2, kind, 0)
            },
        ]
    };
    let negative = fields(next(1, DataKind::Int, 8), DataKind::Bytes);
    assert!(matches!(
        decode(&[], negative, &[SID, 0xF8]),
        Err(DecodeError::InvalidValue { field: 2, .. })
    ));
    let fraction = fields(
        with_compu(
            next(1, DataKind::Uint, 8),
            CompuT::CompuLinear(Box::new(CompuLinearT {
                numerator: 1.0,
                denominator: 2.0,
                offset: 0.0,
            })),
        ),
        DataKind::Bytes,
    );
    assert!(matches!(
        decode(&[], fraction, &[SID, 3]),
        Err(DecodeError::InvalidValue { field: 2, .. })
    ));
    // A whole-number float is a length: 16 / 2 = 8 bits.
    let half = fields(
        with_compu(
            next(1, DataKind::Uint, 8),
            CompuT::CompuLinear(Box::new(CompuLinearT {
                numerator: 1.0,
                denominator: 2.0,
                offset: 0.0,
            })),
        ),
        DataKind::Bytes,
    );
    assert_eq!(
        values(decode(&[], half, &[SID, 16, 0x42])),
        vec![Float(8.0), Bytes(vec![0x42])]
    );
    // 65 bits is no integer width.
    let wide = fields(next(1, DataKind::Uint, 8), DataKind::Uint);
    assert!(matches!(
        decode(&[], wide, &[SID, 65, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        Err(DecodeError::InvalidValue { field: 2, .. })
    ));
    // 12 bits do not cover whole bytes.
    let partial = fields(next(1, DataKind::Uint, 8), DataKind::Bytes);
    assert!(matches!(
        decode(&[], partial, &[SID, 12, 0, 0]),
        Err(DecodeError::InvalidValue { field: 2, .. })
    ));
}

#[test]
fn a_length_field_must_be_decoded_first() {
    let fields = vec![FieldT {
        length_kind: LengthKind::FromField,
        length_field_id: 9,
        ..next(2, DataKind::Bytes, 0)
    }];
    assert!(matches!(
        decode(&[], fields, &[SID, 1]),
        Err(DecodeError::InvalidPlan { field: 2, .. })
    ));
}

#[test]
fn a_leading_length_counts_bytes_after_its_prefix() {
    let leading = |id, kind, bits| FieldT {
        length_kind: LengthKind::LeadingLength,
        ..next(id, kind, bits)
    };
    let fields = vec![
        leading(1, DataKind::Ascii, 8),
        // A 4-bit prefix at bit position 4: the value starts at the next byte edge.
        FieldT {
            bit_offset: 4,
            ..leading(2, DataKind::Bytes, 4)
        },
        little(leading(3, DataKind::Unicode2, 16)),
        next(4, DataKind::Uint, 8),
    ];
    let message = [
        SID, 2, b'o', b'k', 0x10, 0xEE, 4, 0, 0x41, 0, 0x30, 0x01, 0x99,
    ];
    assert_eq!(
        values(decode(&[], fields, &message)),
        vec![
            Text("ok".into()),
            Bytes(vec![0xEE]),
            Text("A\u{130}".into()),
            Unsigned(0x99),
        ]
    );
    let short = vec![leading(1, DataKind::Bytes, 8)];
    assert_eq!(
        decode(&[], short, &[SID, 3, 1, 2]),
        Err(DecodeError::Truncated { field: 1 })
    );
}

fn terminated(id: u32, kind: DataKind, termination: Termination, min: u32, max: u32) -> FieldT {
    FieldT {
        length_kind: LengthKind::Terminated,
        termination,
        min_bytes: min,
        max_bytes: max,
        ..next(id, kind, 0)
    }
}

#[test]
fn a_terminator_ends_the_value_and_is_consumed() {
    let fields = vec![
        terminated(1, DataKind::Ascii, Termination::Zero, 0, 0),
        next(2, DataKind::Uint, 8),
    ];
    assert_eq!(
        values(decode(&[], fields, &[SID, b'h', b'i', 0, 0x42])),
        vec![Text("hi".into()), Unsigned(0x42)]
    );
}

#[test]
fn a_terminator_within_min_bytes_is_data() {
    let fields = vec![terminated(1, DataKind::Bytes, Termination::HexFF, 2, 0)];
    assert_eq!(
        values(decode(&[], fields, &[SID, 0xFF, 0x01, 0x02, 0xFF, 0x03])),
        vec![Bytes(vec![0xFF, 0x01, 0x02])]
    );
}

#[test]
fn a_terminated_value_ends_at_max_bytes_or_the_end_of_the_message() {
    let fields = || {
        vec![
            terminated(1, DataKind::Bytes, Termination::Zero, 0, 3),
            next(2, DataKind::Uint, 8),
        ]
    };
    // No terminator within max_bytes: three bytes, and the next field follows them.
    assert_eq!(
        values(decode(&[], fields(), &[SID, 1, 2, 3, 4])),
        vec![Bytes(vec![1, 2, 3]), Unsigned(4)]
    );
    // The message ends first.
    let fields = vec![terminated(1, DataKind::Bytes, Termination::Zero, 0, 0)];
    assert_eq!(
        values(decode(&[], fields, &[SID, 1, 2])),
        vec![Bytes(vec![1, 2])]
    );
    let fields = vec![terminated(1, DataKind::Bytes, Termination::EndOfPdu, 0, 2)];
    assert_eq!(
        values(decode(&[], fields, &[SID, 0, 0, 0])),
        vec![Bytes(vec![0, 0])]
    );
    let fields = vec![terminated(1, DataKind::Bytes, Termination::EndOfPdu, 0, 0)];
    assert_eq!(
        values(decode(&[], fields, &[SID, 0, 1, 0])),
        vec![Bytes(vec![0, 1, 0])]
    );
}

#[test]
fn a_terminated_value_shorter_than_min_bytes_is_truncated() {
    let fields = vec![terminated(1, DataKind::Bytes, Termination::EndOfPdu, 3, 0)];
    assert_eq!(
        decode(&[], fields, &[SID, 1, 2]),
        Err(DecodeError::Truncated { field: 1 })
    );
}

#[test]
fn a_unicode2_terminator_is_a_whole_character() {
    // "Āa" is 01 00 00 61: the two zero bytes in the middle are not a 0x0000 character.
    let fields = vec![
        terminated(1, DataKind::Unicode2, Termination::Zero, 0, 0),
        next(2, DataKind::Uint, 8),
    ];
    let message = [SID, 0x01, 0x00, 0x00, 0x61, 0x00, 0x00, 0x07];
    assert_eq!(
        values(decode(&[], fields, &message)),
        vec![Text("\u{100}a".into()), Unsigned(7)]
    );
    let odd = vec![terminated(
        1,
        DataKind::Unicode2,
        Termination::EndOfPdu,
        0,
        0,
    )];
    assert!(matches!(
        decode(&[], odd, &[SID, 0x00, 0x41, 0x00]),
        Err(DecodeError::InvalidValue { field: 1, .. })
    ));
}

// ---------------------------------------------------------------- Strings

#[test]
fn strings_are_checked_against_their_encoding() {
    // Every byte is the code point of the same value.
    assert_eq!(
        values(decode(
            &[],
            vec![next(1, DataKind::Ascii, 16)],
            &[SID, b'A', 0xE9]
        )),
        vec![Text("A\u{E9}".into())]
    );
    assert_eq!(
        values(decode(
            &[],
            vec![next(1, DataKind::Utf8, 16)],
            &[SID, 0xC3, 0xA9]
        )),
        vec![Text("\u{E9}".into())]
    );
    assert!(matches!(
        decode(&[], vec![next(1, DataKind::Utf8, 8)], &[SID, 0xC3]),
        Err(DecodeError::InvalidValue { field: 1, .. })
    ));
    assert!(matches!(
        decode(
            &[],
            vec![next(1, DataKind::Unicode2, 16)],
            &[SID, 0xD8, 0x00]
        ),
        Err(DecodeError::InvalidValue { field: 1, .. })
    ));
}

// ---------------------------------------------------------------- Plans the decoder rejects

#[test]
fn plans_that_break_the_schema_rules_are_rejected() {
    let cases: Vec<(&str, FieldT)> = vec![
        ("a 16-bit float", next(1, DataKind::Float, 16)),
        ("a 65-bit integer", next(1, DataKind::Uint, 65)),
        ("a zero-width integer", next(1, DataKind::Int, 0)),
        ("a 6-bit BCD", next(1, DataKind::Bcd, 6)),
        ("bytes off a byte edge", at(1, DataKind::Bytes, 9, 8)),
        ("a partial byte", next(1, DataKind::Bytes, 12)),
        ("half a Unicode2 character", next(1, DataKind::Unicode2, 8)),
        (
            "an AfterPrevious bit position of 8",
            FieldT {
                bit_offset: 8,
                ..next(1, DataKind::Uint, 8)
            },
        ),
        (
            "a leading length on an integer",
            FieldT {
                length_kind: LengthKind::LeadingLength,
                ..next(1, DataKind::Uint, 8)
            },
        ),
        (
            "a terminated integer",
            terminated(1, DataKind::Uint, Termination::Zero, 0, 0),
        ),
        (
            "a 33-bit leading length",
            FieldT {
                length_kind: LengthKind::LeadingLength,
                ..next(1, DataKind::Bytes, 33)
            },
        ),
        (
            "a terminated value off a byte edge",
            FieldT {
                bit_offset: 3,
                ..terminated(1, DataKind::Bytes, Termination::Zero, 0, 0)
            },
        ),
        (
            "min_bytes above max_bytes",
            terminated(1, DataKind::Bytes, Termination::Zero, 3, 2),
        ),
        (
            "an odd Unicode2 max_bytes",
            terminated(1, DataKind::Unicode2, Termination::Zero, 0, 3),
        ),
    ];
    for (case, field) in cases {
        let result = decode(&[], vec![field], &[SID; 16]);
        assert!(
            matches!(result, Err(DecodeError::InvalidPlan { field: 1, .. })),
            "{case}: {result:?}"
        );
    }
}

// ---------------------------------------------------------------- Nested layouts (ADR-242, ADR-302)

fn layout(id: u32, byte_size: u32, fields: Vec<FieldT>) -> LayoutT {
    LayoutT {
        id,
        short_name: Some(format!("l{id}")),
        byte_size,
        fields: Some(fields),
    }
}

fn shaped(id: u32, shape: ShapeT) -> FieldT {
    FieldT {
        shape,
        ..next(id, DataKind::Bytes, 0)
    }
}

fn group(id: u32, layout_id: u32) -> FieldT {
    shaped(id, ShapeT::Group(Box::new(GroupT { layout_id })))
}

fn repeat(id: u32, repeat: RepeatT) -> FieldT {
    shaped(id, ShapeT::Repeat(Box::new(repeat)))
}

fn select(id: u32, key_field_id: u32, cases: Vec<(i64, i64, u32)>, default: Option<u32>) -> FieldT {
    let cases = cases
        .into_iter()
        .enumerate()
        .map(|(at, (lower, upper, layout_id))| SelectCaseT {
            short_name: Some(format!("c{at}")),
            lower,
            upper,
            layout_id,
        })
        .collect();
    shaped(
        id,
        ShapeT::Select(Box::new(SelectT {
            key_field_id,
            cases: Some(cases),
            has_default: default.is_some(),
            default_layout_id: default.unwrap_or(0),
        })),
    )
}

/// The decoded value of the one field with `id` at the top level.
fn value_of(decoded: &[DecodedField], id: u32) -> &PhysicalValue {
    &decoded
        .iter()
        .find(|d| d.id == id)
        .expect("the field is decoded")
        .value
}

/// The values of a list of decoded fields.
fn plain(fields: &[DecodedField]) -> Vec<PhysicalValue> {
    fields.iter().map(|d| d.value.clone()).collect()
}

#[test]
fn a_group_decodes_its_layout_where_it_starts() {
    // A one-byte length, a string of that many bytes, then a group that follows it: its
    // fields count their positions from the group's first byte.
    let fields = vec![
        next(1, DataKind::Uint, 8),
        FieldT {
            length_kind: LengthKind::FromField,
            length_field_id: 1,
            ..next(2, DataKind::Ascii, 0)
        },
        group(3, 1),
        next(4, DataKind::Uint, 8),
    ];
    let layouts = vec![layout(
        1,
        0,
        vec![at(10, DataKind::Uint, 8, 8), at(11, DataKind::Uint, 0, 8)],
    )];
    let decoded = decode_nested(
        &[],
        fields,
        layouts,
        &[SID, 16, b'A', b'B', 0x0A, 0x0B, 0x0C],
    )
    .unwrap();
    let PhysicalValue::Group(inner) = value_of(&decoded, 3) else {
        panic!("not a group: {decoded:?}");
    };
    assert_eq!(plain(inner), [Unsigned(0x0B), Unsigned(0x0A)]);
    // The group ends after its field that ends last, so the next field reads the third byte.
    assert_eq!(value_of(&decoded, 4), &Unsigned(0x0C));
}

#[test]
fn a_layout_with_a_byte_size_ends_there_and_holds_its_fields() {
    let fields = vec![group(1, 1), next(2, DataKind::Uint, 8)];
    let sized = |inner: Vec<FieldT>| vec![layout(1, 3, inner)];
    let decoded = decode_nested(
        &[],
        fields.clone(),
        sized(vec![next(10, DataKind::Uint, 8)]),
        &[SID, 1, 2, 3, 4],
    )
    .unwrap();
    // The next field starts after the group's three bytes, not after its one field.
    assert_eq!(value_of(&decoded, 2), &Unsigned(4));
    // A field reaching past the layout's size is truncated, even with bytes in the message.
    assert_eq!(
        decode_nested(
            &[],
            fields.clone(),
            sized(vec![at(10, DataKind::Uint, 16, 16)]),
            &[SID, 1, 2, 3, 4, 5]
        ),
        Err(DecodeError::Truncated { field: 10 })
    );
    // A layout larger than what is left of the message is truncated.
    assert_eq!(
        decode_nested(&[], fields, sized(vec![]), &[SID, 1, 2]),
        Err(DecodeError::Truncated { field: 1 })
    );
}

#[test]
fn a_fixed_repeat_has_items_a_fixed_size_apart() {
    let fields = vec![
        repeat(
            1,
            RepeatT {
                layout_id: 1,
                count: RepeatCount::Fixed,
                fixed_count: 3,
                item_byte_size: 2,
                ..Default::default()
            },
        ),
        next(2, DataKind::Uint, 8),
    ];
    // Each item uses one of its two bytes.
    let layouts = || vec![layout(1, 0, vec![at(10, DataKind::Uint, 8, 8)])];
    let decoded =
        decode_nested(&[], fields.clone(), layouts(), &[SID, 0, 1, 0, 2, 0, 3, 9]).unwrap();
    let PhysicalValue::Repeat(items) = value_of(&decoded, 1) else {
        panic!("not a repeat: {decoded:?}");
    };
    let items: Vec<_> = items.iter().map(|item| plain(item)).collect();
    assert_eq!(items, [[Unsigned(1)], [Unsigned(2)], [Unsigned(3)]]);
    assert_eq!(value_of(&decoded, 2), &Unsigned(9));
    // The last item does not fit.
    assert_eq!(
        decode_nested(&[], fields, layouts(), &[SID, 0, 1, 0, 2, 0]),
        Err(DecodeError::Truncated { field: 1 })
    );
}

#[test]
fn a_fixed_item_cannot_reach_past_its_size() {
    let fields = vec![repeat(
        1,
        RepeatT {
            layout_id: 1,
            count: RepeatCount::Fixed,
            fixed_count: 1,
            item_byte_size: 1,
            ..Default::default()
        },
    )];
    let layouts = vec![layout(1, 0, vec![next(10, DataKind::Uint, 16)])];
    assert_eq!(
        decode_nested(&[], fields, layouts, &[SID, 1, 2, 3]),
        Err(DecodeError::Truncated { field: 10 })
    );
}

#[test]
fn a_repeat_count_comes_from_an_earlier_field() {
    let fields = vec![
        next(1, DataKind::Uint, 8),
        repeat(
            2,
            RepeatT {
                layout_id: 1,
                count: RepeatCount::FromField,
                count_field_id: 1,
                ..Default::default()
            },
        ),
        next(3, DataKind::Uint, 8),
    ];
    // Items of one length byte and that many data bytes; the length is the item's own.
    let layouts = vec![layout(
        1,
        0,
        vec![
            next(10, DataKind::Uint, 8),
            FieldT {
                length_kind: LengthKind::FromField,
                length_field_id: 10,
                ..next(11, DataKind::Bytes, 0)
            },
        ],
    )];
    let decoded =
        decode_nested(&[], fields, layouts, &[SID, 2, 8, 0xAA, 16, 0xBB, 0xCC, 7]).unwrap();
    let PhysicalValue::Repeat(items) = value_of(&decoded, 2) else {
        panic!("not a repeat: {decoded:?}");
    };
    let items: Vec<_> = items.iter().map(|item| plain(item)).collect();
    assert_eq!(
        items,
        [
            vec![Unsigned(8), Bytes(vec![0xAA])],
            vec![Unsigned(16), Bytes(vec![0xBB, 0xCC])]
        ]
    );
    assert_eq!(value_of(&decoded, 3), &Unsigned(7));
}

#[test]
fn a_repeat_count_must_be_a_whole_number_decoded_first() {
    let repeat_from = |count_field_id| {
        repeat(
            2,
            RepeatT {
                layout_id: 1,
                count: RepeatCount::FromField,
                count_field_id,
                ..Default::default()
            },
        )
    };
    let layouts = || vec![layout(1, 0, vec![next(10, DataKind::Uint, 8)])];
    assert!(matches!(
        decode_nested(&[], vec![repeat_from(1)], layouts(), &[SID, 1]),
        Err(DecodeError::InvalidPlan { field: 2, .. })
    ));
    let negative = next(1, DataKind::Int, 8);
    assert!(matches!(
        decode_nested(
            &[],
            vec![negative, repeat_from(1)],
            layouts(),
            &[SID, 0xFF, 1]
        ),
        Err(DecodeError::InvalidValue { field: 2, .. })
    ));
    let text = next(1, DataKind::Ascii, 8);
    assert!(matches!(
        decode_nested(&[], vec![text, repeat_from(1)], layouts(), &[SID, b'1', 1]),
        Err(DecodeError::InvalidPlan { field: 2, .. })
    ));
}

#[test]
fn an_end_marker_repeat_stops_at_the_marker_and_leaves_it() {
    let fields = vec![
        repeat(
            1,
            RepeatT {
                layout_id: 1,
                count: RepeatCount::UntilMarker,
                marker: Some(Box::new(at(9, DataKind::Uint, 0, 8))),
                marker_lower: 0xFF,
                marker_upper: 0xFF,
                ..Default::default()
            },
        ),
        next(2, DataKind::Uint, 8),
    ];
    let layouts = || vec![layout(1, 0, vec![next(10, DataKind::Uint, 16)])];
    let decoded = decode_nested(&[], fields.clone(), layouts(), &[SID, 0, 1, 0, 2, 0xFF]).unwrap();
    let PhysicalValue::Repeat(items) = value_of(&decoded, 1) else {
        panic!("not a repeat: {decoded:?}");
    };
    assert_eq!(items.len(), 2);
    // The marker's byte is left for the next field.
    assert_eq!(value_of(&decoded, 2), &Unsigned(0xFF));
    // Without a marker the items run to the end of the message.
    assert_eq!(
        decode_nested(&[], fields, layouts(), &[SID, 0, 1, 0, 2]),
        Err(DecodeError::Truncated { field: 2 })
    );
}

#[test]
fn a_byte_string_marker_is_compared_as_a_big_endian_integer() {
    let fields = vec![repeat(
        1,
        RepeatT {
            layout_id: 1,
            count: RepeatCount::UntilMarker,
            marker: Some(Box::new(at(9, DataKind::Bytes, 0, 16))),
            marker_lower: 0xFFFE,
            marker_upper: 0xFFFE,
            ..Default::default()
        },
    )];
    let layouts = vec![layout(1, 0, vec![next(10, DataKind::Uint, 8)])];
    let decoded = decode_nested(&[], fields, layouts, &[SID, 1, 0xFF, 0xFF, 0xFE]).unwrap();
    let PhysicalValue::Repeat(items) = value_of(&decoded, 1) else {
        panic!("not a repeat: {decoded:?}");
    };
    let items: Vec<_> = items.iter().map(|item| plain(item)).collect();
    // 0xFF 0xFF is no marker, so 0xFF is an item; the marker is 0xFF 0xFE.
    assert_eq!(items, [[Unsigned(1)], [Unsigned(0xFF)]]);
}

#[test]
fn an_end_of_pdu_repeat_runs_to_the_end_of_the_message() {
    let fields = vec![
        next(1, DataKind::Uint, 8),
        repeat(
            2,
            RepeatT {
                layout_id: 1,
                count: RepeatCount::UntilEndOfPdu,
                ..Default::default()
            },
        ),
    ];
    let layouts = vec![layout(1, 0, vec![next(10, DataKind::Uint, 16)])];
    let decoded = decode_nested(&[], fields, layouts, &[SID, 7, 0, 1, 0, 2, 0, 3]).unwrap();
    let PhysicalValue::Repeat(items) = value_of(&decoded, 2) else {
        panic!("not a repeat: {decoded:?}");
    };
    let items: Vec<_> = items.iter().map(|item| plain(item)).collect();
    assert_eq!(items, [[Unsigned(1)], [Unsigned(2)], [Unsigned(3)]]);
}

#[test]
fn an_item_that_consumes_no_bytes_ends_the_repeat() {
    for count in [RepeatCount::FromField, RepeatCount::UntilEndOfPdu] {
        let fields = vec![
            next(1, DataKind::Uint, 8),
            repeat(
                2,
                RepeatT {
                    layout_id: 1,
                    count,
                    count_field_id: 1,
                    ..Default::default()
                },
            ),
        ];
        let layouts = vec![layout(1, 0, vec![])];
        let decoded = decode_nested(&[], fields, layouts, &[SID, 200, 1, 2]).unwrap();
        assert_eq!(
            value_of(&decoded, 2),
            &PhysicalValue::Repeat(vec![]),
            "{count:?}"
        );
    }
}

#[test]
fn a_select_decodes_the_case_its_key_chooses() {
    let fields = |default| {
        vec![
            next(1, DataKind::Uint, 8),
            select(2, 1, vec![(0, 0, 0), (3, 4, 1), (5, 5, 2)], default),
            next(3, DataKind::Uint, 8),
        ]
    };
    let layouts = || {
        vec![
            layout(1, 0, vec![next(10, DataKind::Uint, 16)]),
            layout(2, 0, vec![next(20, DataKind::Uint, 8)]),
        ]
    };
    let decoded = decode_nested(&[], fields(None), layouts(), &[SID, 4, 0x12, 0x34, 9]).unwrap();
    assert_eq!(
        value_of(&decoded, 2),
        &PhysicalValue::Select {
            case: Some(1),
            fields: vec![DecodedField {
                id: 10,
                short_name: "f10".into(),
                value: Unsigned(0x1234),
                text_id: 0,
                unit_id: 0,
            }],
        }
    );
    assert_eq!(value_of(&decoded, 3), &Unsigned(9));
    // A case without a layout carries no data.
    let decoded = decode_nested(&[], fields(None), layouts(), &[SID, 0, 9]).unwrap();
    assert_eq!(
        value_of(&decoded, 2),
        &PhysicalValue::Select {
            case: Some(0),
            fields: vec![]
        }
    );
    assert_eq!(value_of(&decoded, 3), &Unsigned(9));
    // An unmatched key takes the default, or is rejected without one.
    let decoded = decode_nested(&[], fields(Some(2)), layouts(), &[SID, 7, 8, 9]).unwrap();
    let PhysicalValue::Select {
        case: None,
        fields: chosen,
    } = value_of(&decoded, 2)
    else {
        panic!("not the default case: {decoded:?}");
    };
    assert_eq!(plain(chosen), [Unsigned(8)]);
    assert!(matches!(
        decode_nested(&[], fields(None), layouts(), &[SID, 7, 8, 9]),
        Err(DecodeError::InvalidValue { field: 2, .. })
    ));
}

#[test]
fn a_select_key_is_the_internal_value() {
    // The key field's conversion doubles its value; the cases name internal values.
    let fields = vec![
        with_compu(next(1, DataKind::Uint, 8), linear(2.0, 1.0, 0.0)),
        select(2, 1, vec![(3, 3, 1)], None),
    ];
    let layouts = vec![layout(1, 0, vec![next(10, DataKind::Uint, 8)])];
    let decoded = decode_nested(&[], fields, layouts, &[SID, 3, 5]).unwrap();
    assert!(matches!(
        value_of(&decoded, 2),
        PhysicalValue::Select { case: Some(0), .. }
    ));
}

#[test]
fn unsigned_keys_compare_with_limits_stored_bit_for_bit() {
    // A 64-bit unsigned key: the frontend stores 2^64 - 1 as -1 and a range across 2^63 as a
    // positive lower and a negative upper limit (csv-tables 5.4).
    let fields = |cases| vec![next(1, DataKind::Uint, 64), select(2, 1, cases, None)];
    let layouts = || vec![layout(1, 0, vec![])];
    let all_ones = [SID, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    let decoded = decode_nested(&[], fields(vec![(-1, -1, 1)]), layouts(), &all_ones).unwrap();
    assert!(matches!(
        value_of(&decoded, 2),
        PhysicalValue::Select { case: Some(0), .. }
    ));
    let across = vec![(i64::MAX - 1, i64::MIN + 1, 1)];
    let high_bit = [SID, 0x80, 0, 0, 0, 0, 0, 0, 0];
    let decoded = decode_nested(&[], fields(across), layouts(), &high_bit).unwrap();
    assert!(matches!(
        value_of(&decoded, 2),
        PhysicalValue::Select { case: Some(0), .. }
    ));
    // A signed key compares as signed.
    let signed = vec![
        next(1, DataKind::Int, 8),
        select(2, 1, vec![(-2, -1, 1)], None),
    ];
    let decoded = decode_nested(&[], signed, layouts(), &[SID, 0xFF]).unwrap();
    assert!(matches!(
        value_of(&decoded, 2),
        PhysicalValue::Select { case: Some(0), .. }
    ));
}

#[test]
fn a_64_bit_marker_of_all_ones_matches_its_stored_limit() {
    let fields = vec![
        repeat(
            1,
            RepeatT {
                layout_id: 1,
                count: RepeatCount::UntilMarker,
                marker: Some(Box::new(at(9, DataKind::Uint, 0, 64))),
                marker_lower: -1,
                marker_upper: -1,
                ..Default::default()
            },
        ),
        next(2, DataKind::Uint, 8),
    ];
    let layouts = vec![layout(1, 0, vec![next(10, DataKind::Uint, 8)])];
    let mut message = vec![SID, 7];
    message.extend([0xFF; 8]);
    let decoded = decode_nested(&[], fields, layouts, &message).unwrap();
    let PhysicalValue::Repeat(items) = value_of(&decoded, 1) else {
        panic!("not a repeat: {decoded:?}");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(value_of(&decoded, 2), &Unsigned(0xFF));
}

#[test]
fn a_marker_wider_than_64_bits_is_a_broken_plan() {
    let fields = vec![repeat(
        1,
        RepeatT {
            layout_id: 1,
            count: RepeatCount::UntilMarker,
            marker: Some(Box::new(at(9, DataKind::Utf8, 0, 4096 * 8))),
            ..Default::default()
        },
    )];
    let layouts = vec![layout(1, 0, vec![next(10, DataKind::Uint, 8)])];
    assert!(matches!(
        decode_nested(&[], fields, layouts, &[SID, 0xFF, 0xFF]),
        Err(DecodeError::InvalidPlan { field: 1, .. })
    ));
}

#[test]
fn a_nested_select_reads_a_key_from_its_own_layout() {
    // A group holding a key and a select: the cascaded selection of ADR-242 item 3.
    let fields = vec![group(1, 1)];
    let layouts = vec![
        layout(
            1,
            0,
            vec![
                next(10, DataKind::Uint, 8),
                select(11, 10, vec![(1, 1, 2)], None),
            ],
        ),
        layout(2, 0, vec![next(20, DataKind::Ascii, 16)]),
    ];
    let decoded = decode_nested(&[], fields, layouts, &[SID, 1, b'O', b'K']).unwrap();
    let PhysicalValue::Group(inner) = value_of(&decoded, 1) else {
        panic!("not a group: {decoded:?}");
    };
    let PhysicalValue::Select { fields: chosen, .. } = value_of(inner, 11) else {
        panic!("not a select: {inner:?}");
    };
    assert_eq!(plain(chosen), [Text("OK".into())]);
}

#[test]
fn shaped_fields_that_break_the_plan_are_rejected() {
    let unknown_layout = vec![group(1, 9)];
    let off_edge = vec![FieldT {
        bit_offset: 3,
        ..group(1, 1)
    }];
    let zero_size = vec![repeat(
        1,
        RepeatT {
            layout_id: 1,
            count: RepeatCount::Fixed,
            fixed_count: 1,
            ..Default::default()
        },
    )];
    let no_marker = vec![repeat(
        1,
        RepeatT {
            layout_id: 1,
            count: RepeatCount::UntilMarker,
            ..Default::default()
        },
    )];
    let key_missing = vec![select(1, 5, vec![(0, 0, 1)], None)];
    for (case, fields) in [
        ("unknown layout", unknown_layout),
        ("off a byte edge", off_edge),
        ("zero item size", zero_size),
        ("no marker", no_marker),
        ("key not decoded", key_missing),
    ] {
        let layouts = vec![layout(1, 0, vec![next(10, DataKind::Uint, 8)])];
        let result = decode_nested(&[], fields, layouts, &[SID, 1, 2]);
        assert!(
            matches!(result, Err(DecodeError::InvalidPlan { field: 1, .. })),
            "{case}: {result:?}"
        );
    }
}

#[test]
fn layouts_nest_at_most_sixteen_deep() {
    // Layout n holds a group of layout n + 1; the last holds one byte.
    let chain = |depth: u32| {
        (1..=depth)
            .map(|n| {
                let inner = if n == depth {
                    next(100 + n, DataKind::Uint, 8)
                } else {
                    group(100 + n, n + 1)
                };
                layout(n, 0, vec![inner])
            })
            .collect::<Vec<_>>()
    };
    assert!(decode_nested(&[], vec![group(1, 1)], chain(16), &[SID, 1]).is_ok());
    assert_eq!(
        decode_nested(&[], vec![group(1, 1)], chain(17), &[SID, 1]),
        Err(DecodeError::TooDeep { field: 116 })
    );
}

#[test]
fn a_message_decodes_to_at_most_65536_values() {
    // Fixed items of one byte with one field each: n items give 2n + 1 values, each item and
    // its field counting, with the repeat.
    let fixed = |fixed_count| {
        vec![repeat(
            1,
            RepeatT {
                layout_id: 1,
                count: RepeatCount::Fixed,
                fixed_count,
                item_byte_size: 1,
                ..Default::default()
            },
        )]
    };
    let layouts = || vec![layout(1, 0, vec![next(10, DataKind::Uint, 8)])];
    let mut message = vec![SID];
    message.resize(1 + MAX_DECODED_VALUES, 0);
    assert!(decode_nested(&[], fixed(32_767), layouts(), &message).is_ok());
    assert_eq!(
        decode_nested(&[], fixed(32_768), layouts(), &message),
        Err(DecodeError::TooManyValues)
    );
    // A count larger than the message ends at the message rather than looping.
    let short = &message[..100];
    assert_eq!(
        decode_nested(&[], fixed(u32::MAX), layouts(), short),
        Err(DecodeError::Truncated { field: 1 })
    );
}

/// Layout n (from 1) holds two copies of `inner(n)`, both at offset 0, so `depth` layouts
/// decode the innermost one 2^depth times over the same bytes.
fn doubling(depth: u32, innermost: Vec<FieldT>) -> Vec<LayoutT> {
    (1..=depth)
        .map(|n| {
            let fields = if n == depth {
                innermost.clone()
            } else {
                let copy = |id| FieldT {
                    placement: Placement::Absolute,
                    ..group(id, n + 1)
                };
                vec![copy(100 + 2 * n), copy(101 + 2 * n)]
            };
            layout(n, 0, fields)
        })
        .collect()
}

#[test]
fn items_read_over_the_same_bytes_count_toward_the_limit() {
    // Repeats of empty items, decoded 2^14 times over the same bytes: the items count even
    // though their fields are none.
    let empty_items = |id| FieldT {
        placement: Placement::Absolute,
        ..repeat(
            id,
            RepeatT {
                layout_id: 99,
                count: RepeatCount::Fixed,
                fixed_count: 1000,
                item_byte_size: 1,
                ..Default::default()
            },
        )
    };
    let mut layouts = doubling(14, vec![empty_items(1), empty_items(2)]);
    layouts.push(layout(99, 0, vec![]));
    let mut message = vec![SID];
    message.resize(1001, 0);
    let started = std::time::Instant::now();
    assert_eq!(
        decode_nested(&[], vec![group(1, 1)], layouts, &message),
        Err(DecodeError::TooManyValues)
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn names_and_values_read_over_the_same_bytes_are_bounded() {
    // A field with a 64 KiB name, decoded 2^10 times: 64 MiB of names.
    let named = FieldT {
        short_name: Some("n".repeat(64 << 10)),
        ..at(1, DataKind::Uint, 0, 8)
    };
    assert_eq!(
        decode_nested(&[], vec![group(1, 1)], doubling(11, vec![named]), &[SID, 0]),
        Err(DecodeError::TooLarge)
    );
}

#[test]
fn table_entries_looked_at_are_bounded() {
    // A select with unmatched cases and a default, decoded 2^13 times: 2,048 cases each make
    // exactly the budget, one case more goes past it. Only the case count differs, so the
    // error comes from the table budget and not from the value or byte limits.
    let decode = |count: i64| {
        let cases = (0..count).map(|n| (n + 10, n + 10, 0)).collect();
        let innermost = vec![at(5, DataKind::Uint, 0, 8), select(6, 5, cases, Some(0))];
        decode_nested(&[], vec![group(1, 1)], doubling(14, innermost), &[SID, 0])
    };
    assert_eq!(MAX_TABLE_ENTRIES, (1 << 13) * 2048);
    assert!(decode(2048).is_ok());
    assert_eq!(decode(2049), Err(DecodeError::TooLarge));
}

#[test]
fn only_the_entries_looked_at_count_toward_the_table_budget() {
    // The same select as above, but its key matches the first case: 2^13 searches look at one
    // entry each, far below the budget.
    let cases = (0..4096).map(|n| (n, n, 0)).collect();
    let innermost = vec![at(5, DataKind::Uint, 0, 8), select(6, 5, cases, None)];
    assert!(decode_nested(&[], vec![group(1, 1)], doubling(14, innermost), &[SID, 0]).is_ok());
    // A text table and a scale-linear conversion matched by their first entry, likewise.
    let entries = (0..4096)
        .map(|n| TextEntryT {
            lower: n,
            upper: n,
            text_id: 1,
        })
        .collect();
    let text = with_compu(
        at(5, DataKind::Uint, 0, 8),
        CompuT::CompuTextTable(Box::new(CompuTextTableT {
            entries: Some(entries),
            default_text_id: 0,
        })),
    );
    let scales = (0..4096)
        .map(|n| LinearScaleT {
            lower: f64::from(n),
            upper: f64::from(n),
            numerator: 1.0,
            denominator: 1.0,
            offset: 0.0,
        })
        .collect();
    let scaled = with_compu(
        at(6, DataKind::Uint, 0, 8),
        CompuT::CompuScaleLinear(Box::new(CompuScaleLinearT {
            scales: Some(scales),
        })),
    );
    assert!(
        decode_nested(
            &[],
            vec![group(1, 1)],
            doubling(14, vec![text, scaled]),
            &[SID, 0]
        )
        .is_ok()
    );
}

#[test]
fn a_shaped_field_past_the_end_of_its_layout_is_truncated() {
    // An empty group, and a case without a layout, take no bytes but must still start within
    // the message.
    let past = FieldT {
        placement: Placement::Absolute,
        bit_offset: 800,
        ..group(1, 1)
    };
    assert_eq!(
        decode_nested(&[], vec![past], vec![layout(1, 0, vec![])], &[SID]),
        Err(DecodeError::Truncated { field: 1 })
    );
    let past_case = FieldT {
        placement: Placement::Absolute,
        bit_offset: 800,
        ..select(2, 1, vec![(0, 0, 0)], None)
    };
    assert_eq!(
        decode_nested(
            &[],
            vec![next(1, DataKind::Uint, 8), past_case],
            vec![],
            &[SID, 0]
        ),
        Err(DecodeError::Truncated { field: 2 })
    );
}

#[test]
fn bytes_that_are_no_value_of_the_marker_type_are_no_marker() {
    // A UTF-8 marker: 0xFF 0x01 is not UTF-8, so it is item data, not an error.
    let fields = vec![
        repeat(
            1,
            RepeatT {
                layout_id: 1,
                count: RepeatCount::UntilMarker,
                marker: Some(Box::new(at(9, DataKind::Utf8, 0, 16))),
                marker_lower: 0,
                marker_upper: 0,
                ..Default::default()
            },
        ),
        next(2, DataKind::Uint, 16),
    ];
    let layouts = vec![layout(1, 0, vec![next(10, DataKind::Uint, 16)])];
    let decoded = decode_nested(&[], fields, layouts, &[SID, 0xFF, 0x01, 0, 0]).unwrap();
    let PhysicalValue::Repeat(items) = value_of(&decoded, 1) else {
        panic!("not a repeat: {decoded:?}");
    };
    let items: Vec<_> = items.iter().map(|item| plain(item)).collect();
    assert_eq!(items, [[Unsigned(0xFF01)]]);
    assert_eq!(value_of(&decoded, 2), &Unsigned(0));
}

#[test]
fn a_dtc_list_follows_nested_layouts_too() {
    let snapshot = vec![
        next(1, DataKind::Uint, 8),
        repeat(
            2,
            RepeatT {
                layout_id: 1,
                count: RepeatCount::FromField,
                count_field_id: 1,
                ..Default::default()
            },
        ),
    ];
    let document = EcuDocumentT {
        schema_version: IR_SCHEMA_VERSION,
        source_format: Some("csv-starlark".into()),
        source_digest: Some("00".into()),
        variant: Some(Box::new(EcuVariantT {
            id: 1,
            dtcs: Some(vec![DtcDefinitionT {
                code: 1,
                snapshot_fields: Some(snapshot),
                ..Default::default()
            }]),
            layouts: Some(vec![layout(1, 0, vec![next(10, DataKind::Uint, 8)])]),
            ..Default::default()
        })),
        procedure: None,
    };
    let buf = encode_ecu_document(&document);
    let document = load_ecu_document(&buf).expect("test document loads");
    let variant = document.variant().unwrap();
    let dtc = variant.dtcs().unwrap().get(0);
    let decoded = decode_dtc_fields(
        &dtc,
        variant.layouts(),
        DtcFields::Snapshot,
        &[0x59, 2, 5, 6],
        1,
    )
    .unwrap();
    let PhysicalValue::Repeat(items) = value_of(&decoded, 2) else {
        panic!("not a repeat: {decoded:?}");
    };
    assert_eq!(items.len(), 2);
}

// ---------------------------------------------------------------- Conversions

fn uint8(compu: CompuT) -> Vec<FieldT> {
    vec![with_compu(next(1, DataKind::Uint, 8), compu)]
}

fn linear(numerator: f64, denominator: f64, offset: f64) -> CompuT {
    CompuT::CompuLinear(Box::new(CompuLinearT {
        numerator,
        denominator,
        offset,
    }))
}

#[test]
fn identity_keeps_the_internal_value() {
    assert_eq!(
        values(decode(
            &[],
            uint8(CompuT::CompuIdentity(Box::default())),
            &[SID, 9]
        )),
        vec![Unsigned(9)]
    );
    // No conversion at all is the identity too.
    assert_eq!(
        values(decode(&[], uint8(CompuT::NONE), &[SID, 9])),
        vec![Unsigned(9)]
    );
}

#[test]
fn linear_scales_and_offsets() {
    // (200 * 1 + -80) / 2 = 60
    assert_eq!(
        values(decode(&[], uint8(linear(1.0, 2.0, -80.0)), &[SID, 200])),
        vec![Float(60.0)]
    );
    let signed = vec![with_compu(next(1, DataKind::Int, 8), linear(0.5, 1.0, 0.0))];
    assert_eq!(values(decode(&[], signed, &[SID, 0xFC])), vec![Float(-2.0)]);
    // A constant zero denominator is a broken plan, whatever the value.
    assert!(matches!(
        decode(&[], uint8(linear(1.0, 0.0, 0.0)), &[SID, 1]),
        Err(DecodeError::InvalidPlan { field: 1, .. })
    ));
    assert!(matches!(
        decode(&[], uint8(linear(f64::MAX, 1.0, 0.0)), &[SID, 2]),
        Err(DecodeError::Conversion { field: 1, .. })
    ));
}

#[test]
fn scale_linear_ranges_include_both_limits() {
    let scale = |lower, upper, numerator| LinearScaleT {
        lower,
        upper,
        numerator,
        denominator: 1.0,
        offset: 0.0,
    };
    let compu = || {
        CompuT::CompuScaleLinear(Box::new(CompuScaleLinearT {
            scales: Some(vec![scale(0.0, 10.0, 1.0), scale(20.0, 30.0, 2.0)]),
        }))
    };
    for (internal, physical) in [(0, 0.0), (10, 10.0), (20, 40.0), (30, 60.0)] {
        assert_eq!(
            values(decode(&[], uint8(compu()), &[SID, internal])),
            vec![Float(physical)],
            "internal {internal}"
        );
    }
    assert!(matches!(
        decode(&[], uint8(compu()), &[SID, 15]),
        Err(DecodeError::Conversion { field: 1, .. })
    ));
    // A scale-linear conversion without scales is a broken plan, not the value's fault.
    for scales in [None, Some(vec![])] {
        let empty = CompuT::CompuScaleLinear(Box::new(CompuScaleLinearT { scales }));
        assert!(matches!(
            decode(&[], uint8(empty), &[SID, 1]),
            Err(DecodeError::InvalidPlan { field: 1, .. })
        ));
    }
    // Should scales overlap (the CSV format rejects that), the first one applies.
    let overlapping = CompuT::CompuScaleLinear(Box::new(CompuScaleLinearT {
        scales: Some(vec![scale(0.0, 10.0, 1.0), scale(5.0, 10.0, 2.0)]),
    }));
    assert_eq!(
        values(decode(&[], uint8(overlapping), &[SID, 7])),
        vec![Float(7.0)]
    );
}

#[test]
fn a_text_table_keeps_the_value_and_names_the_first_matching_text() {
    let entry = |lower, upper, text_id| TextEntryT {
        lower,
        upper,
        text_id,
    };
    let compu = |default_text_id| {
        CompuT::CompuTextTable(Box::new(CompuTextTableT {
            entries: Some(vec![entry(0, 0, 10), entry(0, 5, 11), entry(5, 9, 12)]),
            default_text_id,
        }))
    };
    let text = |default, internal| {
        decode(&[], uint8(compu(default)), &[SID, internal])
            .map(|d| (d[0].value.clone(), d[0].text_id))
    };
    // Entries overlap; the first match wins.
    assert_eq!(text(0, 0), Ok((Unsigned(0), 10)));
    assert_eq!(text(0, 5), Ok((Unsigned(5), 11)));
    assert_eq!(text(0, 9), Ok((Unsigned(9), 12)));
    assert_eq!(text(99, 200), Ok((Unsigned(200), 99)));
    assert!(matches!(
        text(0, 200),
        Err(DecodeError::Conversion { field: 1, .. })
    ));
    // An unsigned value above i64::MAX is compared as itself, not as a negative number.
    let wide = vec![with_compu(
        next(1, DataKind::Uint, 64),
        CompuT::CompuTextTable(Box::new(CompuTextTableT {
            entries: Some(vec![entry(-1, -1, 10)]),
            default_text_id: 20,
        })),
    )];
    let decoded = decode(
        &[],
        wide,
        &[SID, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    )
    .expect("decodes");
    assert_eq!(decoded[0].text_id, 20);
    // A text table on a float is a broken plan.
    let float = vec![with_compu(next(1, DataKind::Float, 32), compu(0))];
    assert!(matches!(
        decode(&[], float, &[SID, 0, 0, 0, 0]),
        Err(DecodeError::InvalidPlan { field: 1, .. })
    ));
}

#[test]
fn interpolation_is_linear_between_points() {
    let point = |internal, physical| InterpPointT { internal, physical };
    let compu = |points| CompuT::CompuTableInterp(Box::new(CompuTableInterpT { points }));
    let table = || {
        compu(Some(vec![
            point(0.0, 0.0),
            point(10.0, 100.0),
            point(20.0, 50.0),
        ]))
    };
    for (internal, physical) in [(0, 0.0), (5, 50.0), (10, 100.0), (14, 80.0), (20, 50.0)] {
        assert_eq!(
            values(decode(&[], uint8(table()), &[SID, internal])),
            vec![Float(physical)],
            "internal {internal}"
        );
    }
    assert!(matches!(
        decode(&[], uint8(table()), &[SID, 21]),
        Err(DecodeError::Conversion { field: 1, .. })
    ));
    // A point exactly on the table gives its physical value, not a rounded interpolation.
    let tenth = compu(Some(vec![point(0.0, 0.0), point(3.0, 0.1)]));
    assert_eq!(
        values(decode(&[], uint8(tenth), &[SID, 3])),
        vec![Float(0.1)]
    );
    // A wide table does not overflow on the way to a small result.
    let wide = vec![with_compu(
        next(1, DataKind::Int, 8),
        compu(Some(vec![point(-1e300, -1e300), point(1e300, 1e300)])),
    )];
    assert_eq!(values(decode(&[], wide, &[SID, 0])), vec![Float(0.0)]);
    // Nor on the way to the fraction, when the points span the whole float range.
    let widest = vec![with_compu(
        next(1, DataKind::Int, 8),
        compu(Some(vec![point(-f64::MAX, -1.0), point(f64::MAX, 1.0)])),
    )];
    assert_eq!(values(decode(&[], widest, &[SID, 0])), vec![Float(0.0)]);
    // A span of subnormal points does not underflow to zero.
    let tiny = vec![with_compu(
        next(1, DataKind::Int, 8),
        compu(Some(vec![
            point(-f64::from_bits(1), -1.0),
            point(f64::from_bits(1), 1.0),
        ])),
    )];
    assert_eq!(values(decode(&[], tiny, &[SID, 0])), vec![Float(0.0)]);
    // A point's physical value that is not finite is no result, even when hit exactly.
    let infinite = compu(Some(vec![point(0.0, f64::INFINITY), point(10.0, 0.0)]));
    assert!(matches!(
        decode(&[], uint8(infinite), &[SID, 0]),
        Err(DecodeError::Conversion { field: 1, .. })
    ));
    // The whole table is checked, not only the points before the matching segment.
    let late = compu(Some(vec![
        point(0.0, 0.0),
        point(10.0, 10.0),
        point(5.0, 5.0),
    ]));
    assert!(matches!(
        decode(&[], uint8(late), &[SID, 3]),
        Err(DecodeError::InvalidPlan { field: 1, .. })
    ));
    let descending = compu(Some(vec![point(10.0, 0.0), point(0.0, 1.0)]));
    assert!(matches!(
        decode(&[], uint8(descending), &[SID, 5]),
        Err(DecodeError::InvalidPlan { field: 1, .. })
    ));
    let single = compu(Some(vec![point(0.0, 0.0)]));
    assert!(matches!(
        decode(&[], uint8(single), &[SID, 0]),
        Err(DecodeError::InvalidPlan { field: 1, .. })
    ));
}

#[test]
fn a_rational_function_divides_two_polynomials() {
    let compu = |numerator: Vec<f64>, denominator: Vec<f64>| {
        CompuT::CompuRational(Box::new(CompuRationalT {
            numerator_coeffs: Some(numerator),
            denominator_coeffs: Some(denominator),
        }))
    };
    // (1 + 2x + 3x^2) / (2 + 0x) at x = 2: 17 / 2.
    assert_eq!(
        values(decode(
            &[],
            uint8(compu(vec![1.0, 2.0, 3.0], vec![2.0, 0.0])),
            &[SID, 2]
        )),
        vec![Float(8.5)]
    );
    // x - 3 is zero at x = 3.
    assert!(matches!(
        decode(&[], uint8(compu(vec![1.0], vec![-3.0, 1.0])), &[SID, 3]),
        Err(DecodeError::Conversion { field: 1, .. })
    ));
    // An overflow is no result.
    assert!(matches!(
        decode(
            &[],
            uint8(compu(vec![0.0, 0.0, f64::MAX], vec![1.0])),
            &[SID, 200]
        ),
        Err(DecodeError::Conversion { field: 1, .. })
    ));
    assert!(matches!(
        decode(&[], uint8(compu(vec![], vec![1.0])), &[SID, 3]),
        Err(DecodeError::InvalidPlan { field: 1, .. })
    ));
}

#[test]
fn bytecode_is_not_evaluated_yet() {
    let compu = CompuT::CompuBytecode(Box::new(CompuBytecodeT { code_offset: 0 }));
    assert!(matches!(
        decode(&[], uint8(compu), &[SID, 1]),
        Err(DecodeError::Unsupported { field: 1, .. })
    ));
}

#[test]
fn a_numeric_conversion_on_a_string_is_a_broken_plan() {
    let fields = vec![with_compu(
        next(1, DataKind::Ascii, 8),
        linear(1.0, 1.0, 0.0),
    )];
    assert!(matches!(
        decode(&[], fields, &[SID, b'a']),
        Err(DecodeError::InvalidPlan { field: 1, .. })
    ));
}

#[test]
fn the_unit_is_carried() {
    let field = FieldT {
        unit_id: 4,
        ..with_compu(next(1, DataKind::Uint, 8), linear(1.0, 1.0, 0.0))
    };
    let decoded = decode(&[], vec![field], &[SID, 1]).expect("decodes");
    assert_eq!((decoded[0].unit_id, decoded[0].text_id), (4, 0));
}

// ---------------------------------------------------------------- Further corners

#[test]
fn a_64_bit_value_at_bit_position_7_spans_nine_bytes() {
    // Bytes 1..=9; the value is bits 7..71 of the normalized 72-bit number.
    let message = [SID, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x80];
    let big = (0x0001_2345_6789_ABCD_EF80_u128 >> 7) as u64;
    let little_value = (u128::from_le_bytes({
        let mut b = [0u8; 16];
        b[..9].copy_from_slice(&message[1..10]);
        b
    }) >> 7) as u64;
    assert_eq!(
        values(decode(
            &[],
            vec![
                at(1, DataKind::Uint, 8 + 7, 64),
                little(at(2, DataKind::Uint, 8 + 7, 64))
            ],
            &message
        )),
        vec![Unsigned(big), Unsigned(little_value)]
    );
}

#[test]
fn a_little_endian_int_and_a_16_digit_bcd() {
    let mut message = vec![SID, 0xFE, 0xFF];
    message.extend_from_slice(&[0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56]);
    let fields = vec![
        little(next(1, DataKind::Int, 16)),
        next(2, DataKind::Bcd, 64),
    ];
    assert_eq!(
        values(decode(&[], fields, &message)),
        vec![Signed(-2), Unsigned(1_234_567_890_123_456)]
    );
}

#[test]
fn a_unicode2_value_ends_at_an_ffff_character() {
    let fields = vec![
        terminated(1, DataKind::Unicode2, Termination::HexFF, 0, 0),
        next(2, DataKind::Uint, 8),
    ];
    let message = [SID, 0x00, 0xFF, 0xFF, 0x00, 0xFF, 0xFF, 0x05];
    assert_eq!(
        values(decode(&[], fields, &message)),
        vec![Text("\u{FF}\u{FF00}".into()), Unsigned(5)]
    );
}

#[test]
fn utf8_cut_by_max_bytes_is_invalid() {
    let fields = vec![terminated(1, DataKind::Utf8, Termination::Zero, 0, 2)];
    assert!(matches!(
        decode(&[], fields, &[SID, b'a', 0xC3, 0xA9]),
        Err(DecodeError::InvalidValue { field: 1, .. })
    ));
}

#[test]
fn a_cut_off_leading_length_prefix_is_truncated() {
    let fields = vec![FieldT {
        length_kind: LengthKind::LeadingLength,
        ..next(1, DataKind::Bytes, 16)
    }];
    assert_eq!(
        decode(&[], fields, &[SID, 0x00]),
        Err(DecodeError::Truncated { field: 1 })
    );
}

#[test]
fn a_nan_float_passes_through_identity() {
    let mut message = vec![SID];
    message.extend_from_slice(&f64::NAN.to_be_bytes());
    let decoded = values(decode(&[], vec![next(1, DataKind::Float, 64)], &message));
    assert!(matches!(decoded[..], [Float(f)] if f.is_nan()));
    let converted = vec![with_compu(
        next(1, DataKind::Float, 64),
        linear(1.0, 1.0, 0.0),
    )];
    assert!(matches!(
        decode(&[], converted, &message),
        Err(DecodeError::Conversion { field: 1, .. })
    ));
}

// ---------------------------------------------------------------- DTCs

/// Decodes `which` of a DTC with `status`, `snapshot` and `extended` field lists from `message`
/// at `start`, after encoding it in a document and loading that through the checking loader.
fn decode_dtc(
    status: Option<Vec<FieldT>>,
    snapshot: Option<Vec<FieldT>>,
    extended: Option<Vec<FieldT>>,
    which: DtcFields,
    message: &[u8],
    start: usize,
) -> Result<Vec<DecodedField>, DecodeError> {
    let document = EcuDocumentT {
        schema_version: IR_SCHEMA_VERSION,
        source_format: Some("csv-starlark".into()),
        source_digest: Some("00".into()),
        variant: Some(Box::new(EcuVariantT {
            id: 1,
            dtcs: Some(vec![DtcDefinitionT {
                code: 0x01_23_45,
                display_code: Some("P0123".into()),
                status_fields: status,
                snapshot_fields: snapshot,
                extended_fields: extended,
                ..Default::default()
            }]),
            ..Default::default()
        })),
        procedure: None,
    };
    let buf = encode_ecu_document(&document);
    let document = load_ecu_document(&buf).expect("test document loads");
    let variant = document.variant().expect("test document has a variant");
    let dtc = variant
        .dtcs()
        .map(|d| d.get(0))
        .expect("test document has a DTC");
    decode_dtc_fields(&dtc, variant.layouts(), which, message, start)
}

/// Three status bits: bit 0, bit 3 and bit 7 of the status byte.
fn status_bits() -> Vec<FieldT> {
    vec![
        at(1, DataKind::Uint, 0, 1),
        at(2, DataKind::Uint, 3, 1),
        at(3, DataKind::Uint, 7, 1),
    ]
}

#[test]
fn the_status_bits_are_read_from_the_stated_byte() {
    // A status record after the service's echo and mask: DTC 0x012345, status 0x29.
    let message = [0x59, 0x02, 0xFF, 0x01, 0x23, 0x45, 0x29];
    let decoded = decode_dtc(
        Some(status_bits()),
        None,
        None,
        DtcFields::Status,
        &message,
        6,
    )
    .expect("decodes");
    assert_eq!(
        decoded
            .iter()
            .map(|d| (d.id, d.value.clone()))
            .collect::<Vec<_>>(),
        [(1, Unsigned(1)), (2, Unsigned(1)), (3, Unsigned(0))]
    );
    assert_eq!(decoded[0].short_name, "f1");
}

#[test]
fn snapshot_fields_count_their_positions_from_the_start() {
    // After the stated start: a 16-bit value, then a byte scaled by 0.5 that follows it, then
    // a byte at an absolute position 4 bytes in.
    let snapshot = vec![
        at(10, DataKind::Uint, 0, 16),
        with_compu(next(11, DataKind::Uint, 8), linear(1.0, 2.0, 0.0)),
        at(12, DataKind::Uint, 32, 8),
    ];
    let message = [0x59, 0x04, 0xAA, 0xBB, 0x01, 0xF4, 0x64, 0xEE, 0x07];
    assert_eq!(
        values(decode_dtc(
            None,
            Some(snapshot),
            None,
            DtcFields::Snapshot,
            &message,
            4,
        )),
        [Unsigned(500), Float(50.0), Unsigned(7)]
    );
}

#[test]
fn extended_fields_are_their_own_list() {
    let extended = vec![at(20, DataKind::Uint, 0, 8), next(21, DataKind::Uint, 8)];
    let message = [0x59, 0x06, 0x01, 0x23, 0x45, 0x29, 0x01, 0x05, 0x02];
    // The extended list is decoded; the status list, though present, is not.
    let decoded = decode_dtc(
        Some(status_bits()),
        None,
        Some(extended),
        DtcFields::Extended,
        &message,
        7,
    );
    assert_eq!(values(decoded), [Unsigned(5), Unsigned(2)]);
}

#[test]
fn a_list_the_dtc_does_not_have_decodes_to_nothing() {
    for which in [DtcFields::Snapshot, DtcFields::Extended] {
        assert_eq!(
            decode_dtc(Some(status_bits()), None, Some(vec![]), which, &[0x59], 9),
            Ok(Vec::new())
        );
    }
}

#[test]
fn a_dtc_list_past_the_end_of_the_message_is_rejected() {
    let message = [0x59, 0x02, 0xFF, 0x01, 0x23, 0x45];
    // Starting past the end.
    assert_eq!(
        decode_dtc(
            Some(status_bits()),
            None,
            None,
            DtcFields::Status,
            &message,
            7
        ),
        Err(DecodeError::StartPastEnd { start: 7, len: 6 })
    );
    // Starting at the end, with nothing left for the first field.
    assert_eq!(
        decode_dtc(
            Some(status_bits()),
            None,
            None,
            DtcFields::Status,
            &message,
            6
        ),
        Err(DecodeError::Truncated { field: 1 })
    );
    // A field that reaches past the end from a start inside the message.
    let snapshot = vec![at(10, DataKind::Uint, 0, 16)];
    assert_eq!(
        decode_dtc(None, Some(snapshot), None, DtcFields::Snapshot, &message, 5),
        Err(DecodeError::Truncated { field: 10 })
    );
}

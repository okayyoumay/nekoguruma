use super::*;
use crate::IR_SCHEMA_VERSION;
use crate::declaration::{
    EcuDocumentT, EcuVariantT, FieldT, LayoutT, PositiveResponseT, RequestT, ServiceT,
    encode_ecu_document, load_ecu_document,
};
use crate::decode::{PhysicalValue, decode_positive_response};

const SID: u8 = 0x2E;

/// A document whose one service has a request and a positive response with SID `SID`, `prefix`
/// and `fields`, so a request's operand can be read back through the decoder.
fn document(prefix: &[u8], fields: Vec<FieldT>) -> Vec<u8> {
    let service = ServiceT {
        id: 1,
        short_name: Some("s".into()),
        request: Some(Box::new(RequestT {
            sid: SID,
            prefix: Some(prefix.to_vec()),
            params: Some(fields.clone()),
        })),
        positive: Some(Box::new(PositiveResponseT {
            sid: SID,
            prefix: Some(prefix.to_vec()),
            fields: Some(fields),
        })),
        ..Default::default()
    };
    encode_ecu_document(&EcuDocumentT {
        schema_version: IR_SCHEMA_VERSION,
        source_format: Some("csv-starlark".into()),
        source_digest: Some("00".into()),
        variant: Some(Box::new(EcuVariantT {
            id: 1,
            services: Some(vec![service]),
            layouts: Some(Vec::<LayoutT>::new()),
            ..Default::default()
        })),
        procedure: None,
    })
}

/// Encodes `values` through the request of [`document`].
fn encode(
    prefix: &[u8],
    fields: Vec<FieldT>,
    values: &[(&str, ParamValue<'_>)],
) -> Result<Vec<u8>, EncodeError> {
    let buf = document(prefix, fields);
    let document = load_ecu_document(&buf).expect("the test document loads");
    let request = document
        .variant()
        .and_then(|v| v.services())
        .map(|s| s.get(0))
        .and_then(|s| s.request())
        .expect("a request");
    encode_request(&request, &values.iter().copied().collect())
}

/// Decodes `operand`, after the SID, through the same fields as a positive response.
fn decode(prefix: &[u8], fields: Vec<FieldT>, operand: &[u8]) -> Vec<PhysicalValue> {
    let buf = document(prefix, fields);
    let document = load_ecu_document(&buf).expect("the test document loads");
    let variant = document.variant().expect("a variant");
    let positive = variant
        .services()
        .map(|s| s.get(0))
        .and_then(|s| s.positive())
        .expect("a positive response");
    let mut message = vec![SID];
    message.extend_from_slice(operand);
    decode_positive_response(&positive, variant.layouts(), &message)
        .expect("decodes")
        .into_iter()
        .map(|d| d.value)
        .collect()
}

/// A `Fixed` field named `name` at an absolute bit offset.
fn at(name: &str, kind: DataKind, bit_offset: u32, bit_length: u32) -> FieldT {
    FieldT {
        id: 1 + u32::from(name.as_bytes()[0]),
        short_name: Some(name.into()),
        bit_offset,
        bit_length,
        kind,
        ..Default::default()
    }
}

fn next(name: &str, kind: DataKind, bit_length: u32) -> FieldT {
    FieldT {
        placement: Placement::AfterPrevious,
        ..at(name, kind, 0, bit_length)
    }
}

fn little(field: FieldT) -> FieldT {
    FieldT {
        byte_order: ByteOrder::Little,
        ..field
    }
}

use ParamValue::{Bytes, Float, Int, Text};

#[test]
fn integers_are_written_after_the_prefix_in_either_byte_order() {
    // A DID in the prefix, then a big-endian, a little-endian, a signed and a BCD value.
    let fields = vec![
        next("a", DataKind::Uint, 16),
        little(next("b", DataKind::Uint, 16)),
        next("c", DataKind::Int, 8),
        next("d", DataKind::Bcd, 16),
    ];
    let values = [
        ("a", Int(0x1234)),
        ("b", Int(0x1234)),
        ("c", Int(-2)),
        ("d", Int(1234)),
    ];
    let operand = encode(&[0xF1, 0x90], fields.clone(), &values).expect("encodes");
    assert_eq!(
        operand,
        [0xF1, 0x90, 0x12, 0x34, 0x34, 0x12, 0xFE, 0x12, 0x34]
    );
    assert_eq!(
        decode(&[0xF1, 0x90], fields, &operand),
        [
            PhysicalValue::Unsigned(0x1234),
            PhysicalValue::Unsigned(0x1234),
            PhysicalValue::Signed(-2),
            PhysicalValue::Unsigned(1234),
        ]
    );
}

#[test]
fn bit_fields_share_a_byte_and_keep_each_other() {
    // Two 4-bit values in byte 1 (the byte after the SID), and one across a byte edge.
    let fields = vec![
        at("h", DataKind::Uint, 8 + 4, 4),
        at("l", DataKind::Uint, 8, 4),
        at("x", DataKind::Uint, 16 + 4, 8),
    ];
    let values = [("h", Int(0xA)), ("l", Int(0x5)), ("x", Int(0xBC))];
    let operand = encode(&[], fields.clone(), &values).expect("encodes");
    // Big-endian: 0xBC four bits up from byte 2 spans bytes 2 and 3 as 0B C0.
    assert_eq!(operand, [0xA5, 0x0B, 0xC0]);
    assert_eq!(
        decode(&[], fields, &operand),
        [
            PhysicalValue::Unsigned(0xA),
            PhysicalValue::Unsigned(0x5),
            PhysicalValue::Unsigned(0xBC),
        ]
    );
}

#[test]
fn integers_of_full_width_and_floats_round_trip() {
    let fields = vec![
        next("u", DataKind::Uint, 64),
        next("i", DataKind::Int, 64),
        next("f", DataKind::Float, 32),
        little(next("g", DataKind::Float, 64)),
    ];
    let values = [
        ("u", Int(-1i64 as u64 as i64 & i64::MAX)),
        ("i", Int(i64::MIN)),
        ("f", Float(1.5)),
        ("g", Int(-3)),
    ];
    let operand = encode(&[], fields.clone(), &values).expect("encodes");
    assert_eq!(
        decode(&[], fields, &operand),
        [
            PhysicalValue::Unsigned(i64::MAX as u64),
            PhysicalValue::Signed(i64::MIN),
            PhysicalValue::Float(1.5),
            PhysicalValue::Float(-3.0),
        ]
    );
}

#[test]
fn strings_of_every_length_kind_round_trip() {
    let leading = FieldT {
        length_kind: LengthKind::LeadingLength,
        ..next("lead", DataKind::Utf8, 8)
    };
    let zero = FieldT {
        length_kind: LengthKind::Terminated,
        termination: Termination::Zero,
        ..next("zero", DataKind::Ascii, 0)
    };
    let wide = little(next("wide", DataKind::Unicode2, 32));
    let rest = FieldT {
        length_kind: LengthKind::Terminated,
        termination: Termination::EndOfPdu,
        ..next("rest", DataKind::Bytes, 0)
    };
    let fields = vec![leading, zero, wide, rest];
    let values = [
        ("lead", Text("é")),
        ("zero", Text("A\u{e9}")),
        ("wide", Text("ÄB")),
        ("rest", Bytes(&[1, 2, 3])),
    ];
    let operand = encode(&[], fields.clone(), &values).expect("encodes");
    assert_eq!(
        operand,
        [2, 0xC3, 0xA9, b'A', 0xE9, 0, 0xC4, 0, b'B', 0, 1, 2, 3]
    );
    assert_eq!(
        decode(&[], fields, &operand),
        [
            PhysicalValue::Text("é".into()),
            PhysicalValue::Text("A\u{e9}".into()),
            PhysicalValue::Text("ÄB".into()),
            PhysicalValue::Bytes(vec![1, 2, 3]),
        ]
    );
}

#[test]
fn a_terminated_value_that_fills_max_bytes_has_no_terminator() {
    let field = |name: &str| FieldT {
        length_kind: LengthKind::Terminated,
        termination: Termination::HexFF,
        max_bytes: 2,
        ..next(name, DataKind::Bytes, 0)
    };
    let fields = vec![field("a"), field("b")];
    let values = [("a", Bytes(&[1, 2])), ("b", Bytes(&[3]))];
    let operand = encode(&[], fields.clone(), &values).expect("encodes");
    assert_eq!(operand, [1, 2, 3, 0xFF]);
    assert_eq!(
        decode(&[], fields, &operand),
        [
            PhysicalValue::Bytes(vec![1, 2]),
            PhysicalValue::Bytes(vec![3]),
        ]
    );
}

#[test]
fn every_field_needs_a_value_and_every_value_a_field() {
    let fields = vec![next("a", DataKind::Uint, 8), next("b", DataKind::Uint, 8)];
    assert_eq!(
        encode(&[], fields.clone(), &[("a", Int(1))]),
        Err(EncodeError::Missing { field: "b".into() })
    );
    assert_eq!(
        encode(
            &[],
            fields,
            &[("a", Int(1)), ("b", Int(2)), ("z", Int(3)), ("c", Int(4))]
        ),
        Err(EncodeError::Unknown { field: "c".into() })
    );
}

#[test]
fn values_a_field_cannot_hold_are_rejected() {
    let one = |field: FieldT, value: ParamValue<'_>| {
        let name = field.short_name.clone().expect("a name");
        match encode(&[], vec![field], &[(name.as_str(), value)]) {
            Err(EncodeError::OutOfRange { .. }) => "range",
            Err(EncodeError::WrongKind { .. }) => "kind",
            Err(EncodeError::InvalidPlan { .. }) => "plan",
            Err(EncodeError::Unsupported { .. }) => "unsupported",
            Err(other) => panic!("{other:?}"),
            Ok(_) => "ok",
        }
    };
    let terminated = |min: u32, max: u32| FieldT {
        length_kind: LengthKind::Terminated,
        termination: Termination::Zero,
        min_bytes: min,
        max_bytes: max,
        ..next("t", DataKind::Bytes, 0)
    };
    let cases: Vec<(FieldT, ParamValue<'_>, &str)> = vec![
        (next("u", DataKind::Uint, 8), Int(255), "ok"),
        (next("u", DataKind::Uint, 8), Int(256), "range"),
        (next("u", DataKind::Uint, 8), Int(-1), "range"),
        (next("i", DataKind::Int, 8), Int(-128), "ok"),
        (next("i", DataKind::Int, 8), Int(128), "range"),
        (next("d", DataKind::Bcd, 8), Int(100), "range"),
        (next("u", DataKind::Uint, 8), Text("1"), "kind"),
        (next("f", DataKind::Float, 32), Float(0.1), "range"),
        (next("f", DataKind::Float, 64), Int(i64::MAX), "range"),
        (next("b", DataKind::Bytes, 16), Bytes(&[1]), "range"),
        (next("b", DataKind::Bytes, 16), Text("ab"), "kind"),
        (next("a", DataKind::Ascii, 8), Text("\u{100}"), "range"),
        (
            next("w", DataKind::Unicode2, 16),
            Text("\u{1F600}"),
            "range",
        ),
        (terminated(2, 0), Bytes(&[1]), "range"),
        (terminated(0, 2), Bytes(&[1, 2, 3]), "range"),
        (terminated(0, 0), Bytes(&[1, 0, 2]), "range"),
        (terminated(2, 0), Bytes(&[0, 1, 2]), "ok"),
        (
            FieldT {
                length_kind: LengthKind::LeadingLength,
                ..next("l", DataKind::Bytes, 1)
            },
            Bytes(&[1, 2]),
            "range",
        ),
        (next("u", DataKind::Uint, 65), Int(1), "plan"),
        (
            FieldT {
                length_kind: LengthKind::FromField,
                ..next("u", DataKind::Bytes, 0)
            },
            Bytes(&[1]),
            "unsupported",
        ),
    ];
    for (field, value, expected) in cases {
        let shown = format!("{field:?} {value:?}");
        assert_eq!(one(field, value), expected, "{shown}");
    }
}

#[test]
fn a_value_that_runs_to_the_end_must_be_last() {
    let rest = FieldT {
        length_kind: LengthKind::Terminated,
        termination: Termination::EndOfPdu,
        ..next("rest", DataKind::Bytes, 0)
    };
    let fields = vec![rest, next("after", DataKind::Uint, 8)];
    assert!(matches!(
        encode(&[], fields, &[("rest", Bytes(&[1])), ("after", Int(2))]),
        Err(EncodeError::InvalidPlan { .. })
    ));
}

#[test]
fn a_field_placed_too_far_out_is_refused() {
    let far = at("far", DataKind::Uint, u32::MAX - 7, 8);
    assert_eq!(
        encode(&[], vec![far], &[("far", Int(1))]),
        Err(EncodeError::TooLarge)
    );
}

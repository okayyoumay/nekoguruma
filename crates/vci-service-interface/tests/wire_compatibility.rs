//! Wire-format compatibility of messages whose encoding the worker gRPC
//! interface depends on (`src/proto/service.proto`).
//!
//! Each test decodes bytes written out by hand from the protobuf encoding
//! rules, so a renumbered tag, a changed scalar type (for example `sint32` to
//! `int32`) or a lost `optional` makes it fail, not only a broken round trip.
//! The tests also round-trip each value through `prost`, and check that
//! fields an older reader does not know are skipped.

use prost::Message;
use vci_service_interface::{
    ComPrimitiveCtrlData, ComPrimitiveHandle, DataItem, ErrorDetail, EventItem, ParamItem,
    PduComPrimitiveStatus, PduError, PduParamClass, com_primitive_ctrl_data, data_item, event_item,
    param_item,
};

fn decode<M: Message + Default>(bytes: &[u8]) -> M {
    M::decode(bytes).expect("hand-encoded bytes should decode")
}

fn assert_round_trip<M: Message + Default + PartialEq + std::fmt::Debug>(value: &M) {
    let encoded = value.encode_to_vec();
    assert_eq!(&M::decode(encoded.as_slice()).expect("own encoding"), value);
}

/// `ParamItem`'s `param_id` kept tag 1 when it moved into the `id` oneof, so
/// a message written by a client built before `param_name` existed still
/// decodes to the same item.
#[test]
fn a_param_item_from_the_schema_before_name_addressing_still_decodes() {
    let bytes = [
        0x08, 0x10, // 1: param_id = 16
        0x10, 0x03, // 2: com_param_class = PDU_PC_COM
        0x18, 0xF4, 0x03, // 3: unum32 = 500
    ];
    let expected = ParamItem {
        id: Some(param_item::Id::ParamId(16)),
        com_param_class: PduParamClass::PduPcCom as i32,
        param_data: Some(param_item::ParamData::Unum32(500)),
    };
    assert_eq!(decode::<ParamItem>(&bytes), expected);
    assert_round_trip(&expected);
}

/// `param_name` is tag 8, and `snum32` is zigzag-encoded (`sint32`).
#[test]
fn a_param_item_addressed_by_name_with_a_signed_value_decodes() {
    let mut bytes = vec![0x42, 0x08]; // 8: param_name, 8 bytes
    bytes.extend_from_slice(b"CP_P2Max");
    bytes.extend_from_slice(&[
        0x10, 0x01, // 2: com_param_class = PDU_PC_TIMING
        0x20, 0x01, // 4: snum32 = -1 (zigzag 1)
    ]);
    let expected = ParamItem {
        id: Some(param_item::Id::ParamName("CP_P2Max".into())),
        com_param_class: PduParamClass::PduPcTiming as i32,
        param_data: Some(param_item::ParamData::Snum32(-1)),
    };
    assert_eq!(decode::<ParamItem>(&bytes), expected);
    assert_round_trip(&expected);
}

/// A reader built against this schema skips fields a newer writer adds, of
/// any wire type, and keeps the ones it knows.
#[test]
fn unknown_fields_from_a_newer_writer_are_skipped() {
    let bytes = [
        0x08, 0x10, // 1: param_id = 16
        0x78, 0x2A, // 15: unknown varint
        0x10, 0x03, // 2: com_param_class = PDU_PC_COM
        0xA2, 0x01, 0x02, 0xAA, 0xBB, // 20: unknown length-delimited
        0x18, 0xF4, 0x03, // 3: unum32 = 500
        0xAD, 0x02, 0x01, 0x02, 0x03, 0x04, // 37: unknown fixed32
    ];
    assert_eq!(
        decode::<ParamItem>(&bytes),
        ParamItem {
            id: Some(param_item::Id::ParamId(16)),
            com_param_class: PduParamClass::PduPcCom as i32,
            param_data: Some(param_item::ParamData::Unum32(500)),
        }
    );
}

/// ADR-204: `cop_tag` is `optional bytes`, so an absent tag and a supplied
/// empty tag stay distinguishable on the wire.
#[test]
fn an_event_item_keeps_an_absent_and_an_empty_cop_tag_apart() {
    let without_tag = [
        0x0A, 0x09, // 1: cop_handle, 9 bytes
        0x08, 0xE9, 0x07, //   1: module_handle = 1001
        0x10, 0xB9, 0x17, //   2: cll_handle = 3001
        0x18, 0xA1, 0x1F, //   3: cop_handle = 4001
        0x10, 0x92, 0x21, // 2: timestamp = 4242
        0x30, 0x92, 0x80, 0x02, // 6: cop_status = PDU_COPST_FINISHED
    ];
    let expected = EventItem {
        cop_handle: Some(ComPrimitiveHandle {
            module_handle: 1001,
            cll_handle: 3001,
            cop_handle: 4001,
        }),
        timestamp: 4242,
        cop_tag: None,
        data: Some(event_item::Data::CopStatus(
            PduComPrimitiveStatus::PduCopstFinished as i32,
        )),
    };
    assert_eq!(decode::<EventItem>(&without_tag), expected);
    assert_round_trip(&expected);

    let mut with_empty_tag = without_tag.to_vec();
    with_empty_tag.extend_from_slice(&[0x4A, 0x00]); // 9: cop_tag, 0 bytes
    let expected = EventItem {
        cop_tag: Some(vec![]),
        ..expected
    };
    assert_eq!(decode::<EventItem>(&with_empty_tag), expected);
    assert_round_trip(&expected);
}

/// The cycle counts are `sint32`, since negative counts carry meaning (for
/// example cyclic and multiple receive; `docs/rpc-api-guide.md`), and the raw
/// TxFlag bytes are oneof member tag 6.
#[test]
fn com_primitive_control_data_keeps_signed_cycles_and_raw_tx_flags() {
    let bytes = [
        0x08, 0xE8, 0x07, // 1: time = 1000
        0x10, 0x02, // 2: num_send_cycles = 1 (zigzag 2)
        0x18, 0x01, // 3: num_receive_cycles = -1 (zigzag 1)
        0x32, 0x04, 0x00, 0x00, 0x00, 0x01, // 6: tx_flag_raw
    ];
    let expected = ComPrimitiveCtrlData {
        time: 1000,
        num_send_cycles: 1,
        num_receive_cycles: -1,
        temp_param_update: 0,
        expected_response_array: vec![],
        tx_flag: Some(com_primitive_ctrl_data::TxFlag::TxFlagRaw(vec![0, 0, 0, 1])),
    };
    assert_eq!(decode::<ComPrimitiveCtrlData>(&bytes), expected);
    assert_round_trip(&expected);
}

/// `IoCtl` results such as `PDU_IOCTL_READ_VBATT` arrive as `unum32_value`,
/// tag 1 of `DataItem`'s oneof.
#[test]
fn a_data_item_carries_an_unsigned_ioctl_result() {
    let bytes = [0x08, 0xBC, 0x69]; // 1: unum32_value = 13500
    let expected = DataItem {
        data: Some(data_item::Data::Unum32Value(13_500)),
    };
    assert_eq!(decode::<DataItem>(&bytes), expected);
    assert_round_trip(&expected);
}

/// ADR-105: `ErrorDetail`'s `detail_text` is `optional`, so "no text" and an
/// empty text stay distinguishable, and the nested event data may be absent.
#[test]
fn an_error_detail_keeps_an_absent_and_an_empty_detail_text_apart() {
    let without_text = [0x08, 0x01]; // 1: pdu_error = PDU_ERR_FCT_FAILED
    let expected = ErrorDetail {
        pdu_error: PduError::PduErrFctFailed as i32,
        error_event_data: None,
        detail_text: None,
    };
    assert_eq!(decode::<ErrorDetail>(&without_text), expected);
    assert_round_trip(&expected);

    let with_empty_text = [0x08, 0x01, 0x1A, 0x00]; // 3: detail_text, 0 bytes
    let expected = ErrorDetail {
        detail_text: Some(String::new()),
        ..expected
    };
    assert_eq!(decode::<ErrorDetail>(&with_empty_text), expected);
    assert_round_trip(&expected);
}

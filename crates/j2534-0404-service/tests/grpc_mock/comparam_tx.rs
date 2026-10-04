//! Per-protocol `SetComParam` forwarding and TX message composition: ComParam
//! values must reach the adapter (`PassThruConnect` baud rate, `SET_CONFIG`
//! values) exactly as mapped, and `CoptSendrecv`'s payload-only `cop_data`
//! must be composed into the protocol's on-wire message (header construction
//! per ADR-050) and validated against SAE J2534-1's per-protocol TX size
//! ranges.

use serial_test::serial;
use vci_service_interface::{
    ComPrimitiveCtrlData, ExpectedResponseData, ParamItem, SetComParamRequest,
    StartComPrimitiveRequest, TxFlagBit, event_item, param_item,
};

use crate::harness::*;

/// Waits for the next `PduCopstFinished` on an already-open event stream.
/// Used to make a COP ordering deterministic (ADR-067: ComParam resolution
/// now binds at `StartComPrimitive` call time, so a later call's binding is
/// only guaranteed to see an earlier COP's effect if that earlier COP is
/// known to have finished first -- unlike ADR-063/064's live-at-execution
/// design, FIFO enqueue order alone no longer suffices). The stream must be
/// subscribed BEFORE the COP being waited on is started, so its
/// `PduCopstFinished` cannot be missed.
async fn wait_for_cop_finished(
    events: &mut tonic::Streaming<vci_service_interface::EventNotification>,
) {
    assert!(
        wait_for_event(events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "expected a PduCopstFinished event"
    );
}

#[tokio::test]
#[serial]
async fn can_protocol_forwards_bit_timing_and_extended_id_frame() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::BIT_SAMPLE_POINT, 80),
            (j2534_0404::SYNC_JUMP_WIDTH, 15),
        ],
    )
    .await;

    assert_eq!(server.backdoor.baud_rate(MOCK_CHANNEL_ID), 500_000);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BIT_SAMPLE_POINT),
        80
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::SYNC_JUMP_WIDTH),
        15
    );

    // ADR-050: cop_data is payload-only; the service prepends the CAN ID
    // from the UniqueRespIdTable entry's CP_CanPhysReqId. TX_FLAG_CAN_29BIT_ID
    // is derived from CP_CanPhysReqFormat's Table B.13 bit 1, not from the
    // client's TxFlagBits (ADR-062) -- 0x02 marks a 29-bit identifier.
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x18DAF110),
                unum32_param(CP_CAN_PHYS_REQ_FORMAT, 0x02),
            ],
        )],
    )
    .await;
    let payload = vec![0x02, 0x10, 0x03];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    let mut expected = 0x18DAF110_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::CAN
    );
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0),
        j2534_0404::TX_EXTENDED_ID
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn iso15765_protocol_forwards_flow_control_params_and_plain_frame() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::ISO15765_BS, 0),
            (j2534_0404::ISO15765_STMIN, 0),
            (j2534_0404::BS_TX, 8),
            // CP_StMinOverride is in microseconds (ADR-037); 500 us converts
            // exactly to the J2534 STMIN_TX 100 us-resolution encoding 0xF5.
            (j2534_0404::STMIN_TX, 500),
        ],
    )
    .await;

    assert_eq!(server.backdoor.baud_rate(MOCK_CHANNEL_ID), 500_000);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::ISO15765_BS),
        0
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::ISO15765_STMIN),
        0
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BS_TX),
        8
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::STMIN_TX),
        0xF5
    );

    // ADR-050: cop_data is payload-only; the service prepends the CAN ID
    // from the UniqueRespIdTable entry's CP_CanPhysReqId.
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let payload = vec![0x02, 0x10, 0x03];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    let mut expected = 0x7E0_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::ISO15765
    );
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0),
        j2534_0404::TX_NORMAL_TRANSMIT
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn iso15765_extended_id_and_frame_pad_tx_flags_are_forwarded() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // ADR-050: cop_data is payload-only; the service prepends the CAN ID
    // from the UniqueRespIdTable entry's CP_CanPhysReqId. TX_FLAG_CAN_29BIT_ID
    // is derived from CP_CanPhysReqFormat's Table B.13 bit 1, not from the
    // client's TxFlagBits (ADR-062) -- TX_FLAG_ISO15765_FRAME_PAD is
    // unaffected and still comes from the client's request.
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x18DAF110),
                unum32_param(CP_CAN_PHYS_REQ_FORMAT, 0x02),
            ],
        )],
    )
    .await;
    let payload = vec![0x02, 0x3E, 0x00];
    send_data(
        &mut client,
        cll_handle,
        payload.clone(),
        vec![TxFlagBit::TxFlagIso15765FramePad],
    )
    .await;

    let mut data = 0x18DAF110_u32.to_be_bytes().to_vec();
    data.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::ISO15765
    );
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0),
        j2534_0404::TX_EXTENDED_ID | j2534_0404::TX_ISO15765_FRAME_PAD
    );
    // The service forwards cop_data verbatim; padding to the CAN frame length
    // is the adapter's responsibility on real hardware, not the service's.
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), data);

    server.shutdown().await;
}

/// ADR-116: `tx_flag_raw` is the ISO 22900-2 D.2.1 (Table D.4) 4-byte
/// `TxFlag` byte-array layout, byte 0 first -- not a native J2534 `TxFlags`
/// u32. `WAIT_P3_MIN_ONLY` lives at D.2.1 byte 2 bit 1 (`0x02`) and
/// `ISO15765_FRAME_PAD` at byte 3 bit 6 (`0x40`); both must decode to their
/// (differently-positioned) J2534 `TxFlags` bit.
#[tokio::test]
#[serial]
async fn iso15765_frame_pad_and_wait_p3_min_only_decoded_from_raw_tx_flag_iso_d21_layout() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![unum32_param(CP_CAN_PHYS_REQ_ID, 0x18DAF110)],
        )],
    )
    .await;
    let payload = vec![0x02, 0x3E, 0x00];
    send_data_with_raw_tx_flag(
        &mut client,
        cll_handle,
        payload.clone(),
        vec![0x00, 0x00, 0x02, 0x40],
    )
    .await;

    let mut data = 0x18DAF110_u32.to_be_bytes().to_vec();
    data.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0),
        j2534_0404::TX_WAIT_P3_MIN_ONLY | j2534_0404::TX_ISO15765_FRAME_PAD
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), data);

    server.shutdown().await;
}

/// ADR-116/ADR-062: the D.2.1 raw layout's `CAN_29BIT_ID` (byte 2 bit 0) and
/// `ISO15765_ADDR_TYPE` (byte 3 bit 7) positions are not decoded at all --
/// `apply_resolved_tx_flags` always overwrites those two J2534 `TxFlags` bits
/// from the CLL's resolved CAN addressing, so a client raw-requesting the
/// 29-bit-ID bit for an 11-bit-format CLL must NOT see it echoed back.
#[tokio::test]
#[serial]
async fn iso15765_raw_tx_flag_can_addressing_bits_ignored_and_overridden() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // CP_CanPhysReqFormat left at its default (11-bit, bit 1 clear).
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(1, vec![unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0)])],
    )
    .await;
    let payload = vec![0x02, 0x3E, 0x00];
    // Byte 2 bit 0 (CAN_29BIT_ID) and byte 3 bit 7 (ISO15765_ADDR_TYPE) set.
    send_data_with_raw_tx_flag(
        &mut client,
        cll_handle,
        payload.clone(),
        vec![0x00, 0x00, 0x01, 0x80],
    )
    .await;

    let mut data = 0x7E0_u32.to_be_bytes().to_vec();
    data.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0), 0);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), data);

    server.shutdown().await;
}

/// SAE J2534-1's CAN row (Min Tx 4, Max Tx 12: 4-byte CAN ID + up to 8 data
/// bytes) is enforced on CoptSendrecv before the item is queued.
#[tokio::test]
#[serial]
async fn can_protocol_rejects_cop_data_outside_size_range() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // ADR-050/ADR-067: without a UniqueRespIdTable entry, the service has no
    // CAN ID to construct the message from -- rejected synchronously, since
    // resolution binds against a call-time ComParam snapshot in
    // rpc_start_com_primitive itself (ADR-067 reverts ADR-064's deferral).
    let status = send_data_expect_rejected(&mut client, cll_handle, vec![0x02, 0x10]).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("CP_CanPhysReqId"),
        "{}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // Payload of 9 bytes -> 4-byte header + 9 = 13, exceeds the 12-byte max.
    let too_long_payload = vec![0u8; 9];
    let status = send_data_expect_rejected(&mut client, cll_handle, too_long_payload).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("4..=12"), "{}", status.message());
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    // Boundary values: an empty payload (min, 4-byte CAN ID only) and an
    // 8-byte payload (max, 12 bytes total) are both accepted.
    send_data(&mut client, cll_handle, vec![], vec![]).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        0x7E0_u32.to_be_bytes().to_vec()
    );

    let at_max_payload = vec![0u8; 8];
    send_data(&mut client, cll_handle, at_max_payload.clone(), vec![]).await;
    let mut expected_at_max = 0x7E0_u32.to_be_bytes().to_vec();
    expected_at_max.extend_from_slice(&at_max_payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_at_max
    );

    server.shutdown().await;
}

/// SAE J2534-1's ISO15765 row (Min Tx 4, Max Tx 4099 under normal
/// addressing: 4-byte CAN ID + up to 4095 data bytes) is enforced on
/// CoptSendrecv before the item is queued, for a hardware ISO15765 channel
/// with no UniqueRespIdTable entry configured (addressing defaults Normal).
#[tokio::test]
#[serial]
async fn iso15765_protocol_rejects_cop_data_outside_size_range() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // ADR-050/ADR-067: without a UniqueRespIdTable entry, the service has no
    // CAN ID to construct the message from -- rejected synchronously (ADR-067
    // reverts ADR-064's deferral of this check to poll-task execution time).
    let status = send_data_expect_rejected(&mut client, cll_handle, vec![0x02, 0x10]).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("CP_CanPhysReqId"),
        "{}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // Payload of 4096 bytes -> 4-byte header + 4096 = 4100, exceeds normal
    // addressing's 4099-byte max.
    let too_long_payload = vec![0u8; 4096];
    let status = send_data_expect_rejected(&mut client, cll_handle, too_long_payload).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("4..=4099"),
        "{}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    // Boundary values: an empty payload (min, 4-byte CAN ID only) and a
    // 4095-byte payload (max, 4099 bytes total) are both accepted.
    send_data(&mut client, cll_handle, vec![], vec![]).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        0x7E0_u32.to_be_bytes().to_vec()
    );

    let at_max_payload = vec![0u8; 4095];
    send_data(&mut client, cll_handle, at_max_payload.clone(), vec![]).await;
    let mut expected_at_max = 0x7E0_u32.to_be_bytes().to_vec();
    expected_at_max.extend_from_slice(&at_max_payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_at_max
    );

    server.shutdown().await;
}

/// SAE J2534-1's ISO15765 "extended addressing" row (Min Tx 5, Max Tx 4100:
/// 4-byte CAN ID + 1 AE byte + up to 4095 data bytes) applies once the
/// connecting CLL's UniqueRespIdTable marks the request CAN ID as extended
/// (`CP_CanPhysReqFormat` bit 3), widening both bounds by 1 relative to
/// normal addressing.
#[tokio::test]
#[serial]
async fn iso15765_extended_addressing_widens_tx_size_range() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let phys_req_id = 0x7E1_u32;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, phys_req_id),
                unum32_param(CP_CAN_PHYS_REQ_FORMAT, can_id_format::EXTENDED_11BIT),
                unum32_param(CP_CAN_PHYS_REQ_EXT_ADDR, 0xF1),
            ],
        )],
    )
    .await;

    // ADR-050/ADR-067: cop_data is payload-only; the service prepends the CAN
    // ID and AE byte from the UniqueRespIdTable entry configured above. A
    // 4096-byte payload -> 4 + 1 + 4096 = 4101, one byte past the
    // extended-addressing max (4100), and is rejected synchronously (ADR-067
    // reverts ADR-064's deferral of this check to poll-task execution time).
    let too_long_payload = vec![0u8; 4096];
    let status = send_data_expect_rejected(&mut client, cll_handle, too_long_payload).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("5..=4100"),
        "{}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    // A 4095-byte payload -> 4 + 1 + 4095 = 4100, the extended-addressing
    // max, is accepted even though it exceeds normal addressing's 4099 max.
    let at_max_payload = vec![0u8; 4095];
    send_data(&mut client, cll_handle, at_max_payload.clone(), vec![]).await;
    let mut expected = phys_req_id.to_be_bytes().to_vec();
    expected.push(0xF1);
    expected.extend_from_slice(&at_max_payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    server.shutdown().await;
}

/// ADR-062: `TX_FLAG_ISO15765_ADDR_TYPE` is derived from `CP_CanPhysReqFormat`
/// Table B.13 bit 3 (extended addressing), not from the client's TxFlagBits.
/// `EXTENDED_11BIT` sets bit 3 but not bit 1, so only `ISO15765_ADDR_TYPE`
/// should be set, not `CAN_29BIT_ID`.
#[tokio::test]
#[serial]
async fn iso15765_addr_type_tx_flag_derived_from_extended_addressing_format() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E1),
                unum32_param(CP_CAN_PHYS_REQ_FORMAT, can_id_format::EXTENDED_11BIT),
                unum32_param(CP_CAN_PHYS_REQ_EXT_ADDR, 0xF1),
            ],
        )],
    )
    .await;

    send_data(&mut client, cll_handle, vec![0x22, 0xF1, 0x90], vec![]).await;
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0),
        j2534_0404::ISO15765_ADDR_TYPE
    );

    server.shutdown().await;
}

/// ADR-062: `TX_FLAG_CAN_29BIT_ID` is authoritatively derived from
/// `CP_CanPhysReqFormat`, overriding rather than merely supplementing a
/// client's explicit `TxFlagCan29bitId` request -- a client that asks for it
/// while the resolved format says 11-bit must not get the bit set.
#[tokio::test]
#[serial]
async fn can_29bit_id_tx_flag_overrides_client_request_when_format_says_11bit() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // No CP_CanPhysReqFormat set at all -> resolves to 11-bit/Normal, the
    // same as the pre-ADR-062 default.
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    send_data(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        vec![TxFlagBit::TxFlagCan29bitId],
    )
    .await;
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0),
        j2534_0404::TX_NORMAL_TRANSMIT,
        "the client's TxFlagCan29bitId request must be overridden, not honoured, when the resolved \
         CP_CanPhysReqFormat says 11-bit"
    );

    server.shutdown().await;
}

/// ADR-062: `TX_FLAG_SCI_MODE`/`TX_FLAG_SCI_TX_VOLTAGE` are derived from
/// `CP_SCITransmitMode`/`CP_SCISetProgVoltage` -- previously stored-only
/// ComParams with no effect on hardware (ADR-056-era `comparam-protocol-support.md`
/// documented both as "S", not forwarded).
#[tokio::test]
#[serial]
async fn sci_tx_flags_derived_from_transmit_mode_and_prog_voltage_comparams() {
    const CP_SCI_TRANSMIT_MODE: u32 = 0x8091;
    const CP_SCI_SET_PROG_VOLTAGE: u32 = 0x80C2;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::SCI_A_ENGINE,
        &[
            (j2534_0404::DATA_RATE, 7_812),
            (CP_SCI_TRANSMIT_MODE, 1),
            (CP_SCI_SET_PROG_VOLTAGE, 12),
        ],
    )
    .await;

    send_data(&mut client, cll_handle, vec![0x01, 0x02], vec![]).await;
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0),
        j2534_0404::SCI_MODE | j2534_0404::SCI_TX_VOLTAGE
    );

    server.shutdown().await;
}

/// ADR-062: `CP_SCISetProgVoltage` left at its `0xFFFF_FFFF` ("no override")
/// default must not set `TX_FLAG_SCI_TX_VOLTAGE`.
#[tokio::test]
#[serial]
async fn sci_tx_voltage_flag_not_set_when_prog_voltage_left_at_default() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::SCI_A_ENGINE,
        &[(j2534_0404::DATA_RATE, 7_812)],
    )
    .await;

    send_data(&mut client, cll_handle, vec![0x01, 0x02], vec![]).await;
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0),
        j2534_0404::TX_NORMAL_TRANSMIT
    );

    server.shutdown().await;
}

/// ADR-055: ISO 15765-2 requires a functionally addressed (broadcast)
/// request to fit in a single frame -- there is no way to negotiate
/// FlowControl for a multi-frame exchange with an unspecified target. The
/// service rejects an oversized functional payload before it reaches the
/// mock, and accepts one that exactly fits a Single Frame.
#[tokio::test]
#[serial]
async fn iso15765_functional_addressing_rejects_multi_frame_payload() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;

    // Normal addressing's Single Frame max is 7 bytes; 8 bytes would need a
    // FirstFrame + ConsecutiveFrame exchange, which functional addressing
    // cannot support -- rejected synchronously (ADR-067 reverts ADR-064's
    // deferral of this check to poll-task execution time).
    let too_long_payload = vec![0u8; 8];
    let status = send_data_expect_rejected(&mut client, cll_handle, too_long_payload).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("Single Frame"),
        "{}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    // Exactly 7 bytes fits in one Single Frame and is accepted.
    let at_max_payload = vec![0u8; 7];
    send_data(&mut client, cll_handle, at_max_payload.clone(), vec![]).await;
    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&at_max_payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    server.shutdown().await;
}

/// ADR-063/ADR-067: `temp_param_update` must reflect every service-level
/// ComParam this service resolves for message construction, not just the
/// ones with a J2534 `SET_CONFIG` equivalent. `CP_RequestAddrMode` has no
/// hardware mapping at all -- it only selects which CAN ID `tx_header.rs`
/// builds -- so a `SetComParam`-staged switch to functional addressing must
/// take effect for a `temp_param_update` `CoptSendrecv` even though no
/// `CoptUpdateparam` was ever called. This confirms the Working snapshot --
/// not Active -- drives this one send, and (ADR-067 claim D, superseding
/// ADR-063's "Working is never reset") that Working is written back from
/// Active immediately after the call returns: a subsequent plain
/// `CoptSendrecv` reverts to physical addressing because Working now equals
/// Active, not because temp_param_update left Working untouched.
#[tokio::test]
#[serial]
async fn iso15765_temp_param_update_uses_working_request_addr_mode_without_updateparam() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    // Active addressing at this point is physical (CP_RequestAddrMode absent
    // defaults to physical, ADR-054); set the physical CAN ID via the
    // UniqueRespIdTable as usual.
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // Stage (Working only, no CoptUpdateparam) a switch to functional
    // addressing plus the functional CAN ID it needs. Active is untouched.
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;

    // A plain CoptSendrecv (temp_param_update=0) still resolves addressing
    // from Active: physical, unaffected by the staged Working change.
    let payload = vec![0x02, 0x10, 0x03];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;
    let mut expected_physical = 0x7E0_u32.to_be_bytes().to_vec();
    expected_physical.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_physical
    );

    // CoptSendrecv with temp_param_update=1 borrows the Working set for this
    // COP (ADR-063) -- CP_RequestAddrMode=2 + CP_CanFuncReqId now take
    // effect for this one send, without ever calling CoptUpdateparam.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload.clone(),
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut expected_functional = 0x7DF_u32.to_be_bytes().to_vec();
    expected_functional.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_functional
    );

    // ADR-067 claim D: Working was written back from Active as soon as the
    // temp_param_update call returned -- GetComParam now shows Working ==
    // Active (physical) for both params, the opposite of ADR-063's "Working
    // is never promoted or reset" invariant.
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE).await,
        0
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID).await,
        0
    );

    // A subsequent plain CoptSendrecv reverts to physical addressing --
    // Working now equals Active (writeback), not because temp_param_update
    // left Working untouched.
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 3);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 2),
        expected_physical
    );

    server.shutdown().await;
}

/// ADR-067 (superseding ADR-063's FIFO-ordering guarantee): a
/// `temp_param_update` `CoptSendrecv` binds the Working snapshot it resolves
/// from at ITS OWN `StartComPrimitive` call time, not live at poll-task
/// execution time -- so whether an earlier-queued `CoptRestoreParam` (Active
/// -> Working) affects it depends entirely on call ORDER (has the earlier
/// call's effect landed before the later call binds its snapshot?), not
/// FIFO enqueue order. This test makes that deterministic by waiting for the
/// `CoptRestoreParam`'s `PduCopstFinished` event before issuing the temp
/// `CoptSendrecv` -- guaranteeing RestoreParam's Working-mutating effect has
/// already run by the time the temp call binds its snapshot, so the send
/// must be physical.
#[tokio::test]
#[serial]
async fn iso15765_temp_param_update_respects_queued_restore_param_ordering() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // Stage a switch to functional addressing in Working (Active stays
    // physical, since CoptUpdateparam is never called).
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;

    // Subscribe BEFORE starting CoptRestoreParam so its PduCopstFinished
    // cannot be missed.
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(
                vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_handle),
            ),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Queue CoptRestoreParam -- copies Active (physical) back over Working,
    // undoing the staged functional switch -- and wait for it to finish
    // before issuing the temp_param_update CoptSendrecv: ADR-067 binds the
    // temp COP's Working snapshot at ITS OWN call time, so only a call made
    // AFTER RestoreParam is known to have completed is guaranteed to see its
    // effect.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptRestoreParam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptRestoreParam) should succeed");
    wait_for_cop_finished(&mut events).await;

    let payload = vec![0x02, 0x10, 0x03];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload.clone(),
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // RestoreParam finished (and reverted Working to physical) strictly
    // before the temp CoptSendrecv's call-time binding -- the send must NOT
    // be functional.
    let mut expected_physical = 0x7E0_u32.to_be_bytes().to_vec();
    expected_physical.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_physical
    );

    // Drop the still-open event subscription stream before shutting down --
    // otherwise the gRPC server's graceful shutdown waits on it indefinitely.
    drop(events);
    server.shutdown().await;
}

/// ADR-067 (superseding ADR-064's FIFO-ordering guarantee): an ordinary
/// `CoptSendrecv` (`temp_param_update` unset) binds the Active snapshot it
/// resolves from at ITS OWN `StartComPrimitive` call time, not live at
/// poll-task execution time -- so whether an earlier-queued
/// `CoptUpdateparam` (Working -> Active) affects it depends entirely on call
/// ORDER, not FIFO enqueue order. This test makes that deterministic by
/// waiting for the `CoptUpdateparam`'s `PduCopstFinished` event before
/// issuing the plain `CoptSendrecv` -- guaranteeing CoptUpdateparam's
/// promotion has already run by the time the send binds its snapshot, so it
/// must be functional.
#[tokio::test]
#[serial]
async fn iso15765_plain_sendrecv_respects_queued_updateparam_ordering() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // Stage a switch to functional addressing in Working; Active stays
    // physical until CoptUpdateparam promotes it.
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;

    // Subscribe BEFORE starting CoptUpdateparam so its PduCopstFinished
    // cannot be missed.
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(
                vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_handle),
            ),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Queue CoptUpdateparam -- promotes Working (functional) over Active --
    // and wait for it to finish before issuing the plain CoptSendrecv:
    // ADR-067 binds the send's Active snapshot at ITS OWN call time, so only
    // a call made AFTER CoptUpdateparam is known to have completed is
    // guaranteed to see its effect.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptUpdateparam) should succeed");
    wait_for_cop_finished(&mut events).await;

    let payload = vec![0x02, 0x10, 0x03];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    // CoptUpdateparam finished (and promoted Working to Active) strictly
    // before the plain CoptSendrecv's call-time binding -- the send must be
    // functional, not physical.
    let mut expected_functional = 0x7DF_u32.to_be_bytes().to_vec();
    expected_functional.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_functional
    );

    // Drop the still-open event subscription stream before shutting down --
    // otherwise the gRPC server's graceful shutdown waits on it indefinitely.
    drop(events);
    server.shutdown().await;
}

/// SAE J2534-1's J1850PWM row (Min Tx 3, Max Tx 10: 3 header bytes + up to 7
/// data bytes) is enforced on CoptSendrecv before the item is queued.
#[tokio::test]
#[serial]
async fn j1850pwm_protocol_rejects_cop_data_outside_size_range() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850PWM,
        &[(j2534_0404::DATA_RATE, 41_600)],
    )
    .await;

    // ADR-050/ADR-067: cop_data is payload-only; the service constructs the
    // 3-byte J1850 header (format/target/source) from ComParams. A payload of
    // 8 bytes -> 3-byte header + 8 = 11, exceeds the 10-byte max -- rejected
    // synchronously (ADR-067 reverts ADR-064's deferral to poll-task
    // execution time).
    let too_long_payload = vec![0u8; 8];
    let status = send_data_expect_rejected(&mut client, cll_handle, too_long_payload).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("3..=10"), "{}", status.message());
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    // Boundary values: an empty payload (min, 3-byte header only) and a
    // 7-byte payload (max, 10 bytes total) are both accepted.
    send_data(&mut client, cll_handle, vec![], vec![]).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0).len(), 3);

    let at_max_payload = vec![0u8; 7];
    send_data(&mut client, cll_handle, at_max_payload, vec![]).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 1).len(), 10);

    server.shutdown().await;
}

/// SAE J2534-1's ISO14230 row (Min Tx 1, Max Tx 259: up to 4 header bytes +
/// up to 255 data bytes) is enforced on CoptSendrecv before the item is
/// queued. The "Manual Checksum" 260-byte variant is out of scope; this
/// service does not implement that mode.
#[tokio::test]
#[serial]
async fn iso14230_protocol_rejects_cop_data_outside_size_range() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    // ADR-050/ADR-067: cop_data is payload-only; the service constructs a
    // 4-byte KWP2000 header (format/target/source/length) from ComParams. A
    // 256-byte payload -> 4-byte header + 256 = 260, one byte past the
    // 259-byte max -- rejected synchronously (ADR-067 reverts ADR-064's
    // deferral to poll-task execution time). Since ADR-075 the KWP arm of
    // build_tx_message rejects any payload over 255 before the frame is even
    // built (the single KWP length byte cannot encode it -- the check that
    // covers ISO9141's wider 4128-byte frame ceiling too), so that guard
    // fires first here; the frame-size range check remains as
    // defense-in-depth behind it.
    let too_long_payload = vec![0u8; 256];
    let status = send_data_expect_rejected(&mut client, cll_handle, too_long_payload).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("KWP length byte"),
        "{}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    // Max payload (255 bytes) -> 4 + 255 = 259, the max, is accepted.
    let at_max_payload = vec![0u8; 255];
    send_data(&mut client, cll_handle, at_max_payload, vec![]).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0).len(), 259);

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn iso9141_protocol_forwards_kline_timing_params_and_request() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // ADR-072: ComParam-space timing values are microseconds (1 us
    // resolution); P1_MAX/P3_MIN convert to native 0.5 ms steps, the rest to
    // native 1 ms steps -- so e.g. `P1_MAX = 10_000` (10 ms) converts to a
    // native `P1_MAX` of `20` (10 ms / 0.5 ms) and `W1 = 60_000` (60 ms)
    // converts to a native `W1` of `60` (60 ms / 1 ms).
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::PARITY, 0),
            (j2534_0404::P1_MAX, 10_000),
            (j2534_0404::P3_MIN, 27_500),
            (j2534_0404::W0, 300_000),
            (j2534_0404::W1, 60_000),
            (j2534_0404::W2, 20_000),
            (j2534_0404::W3, 55_000),
            (j2534_0404::W4, 5_000),
            (j2534_0404::TIDLE, 300_000),
            (j2534_0404::TINIL, 25_000),
            (j2534_0404::TWUP, 300_000),
        ],
    )
    .await;

    assert_eq!(server.backdoor.baud_rate(MOCK_CHANNEL_ID), 10_400);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::PARITY),
        0
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P1_MAX),
        20
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P3_MIN),
        55
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W0),
        300
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W1),
        60
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W2),
        20
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W3),
        55
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W4),
        5
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TIDLE),
        300
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TINIL),
        25
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TWUP),
        300
    );
    // W5 is ISO14230-only; ISO9141's connect must not forward it, even
    // though CP_TIdle's forwarding derives a W0/W5 entry (`expand_tidle`,
    // ADR-072) -- `expand_tidle` only derives W5 on an ISO14230 link.
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W5),
        0
    );

    // ADR-050: cop_data is payload-only; the service constructs a 4-byte
    // KWP2000 header (format/target/source/length) from ComParams. No
    // CP_PhysReqFormatPriorityType/CP_PhysReqTargetAddr/NODE_ADDRESS were
    // set above, so the header uses this service's documented fallback
    // defaults (0x80 physical / 0x10 target / 0xF1 tester source).
    let payload = vec![0x01, 0x00];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    let mut expected = vec![0x80, 0x10, 0xF1, payload.len() as u8];
    expected.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::ISO9141
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn iso14230_protocol_forwards_kwp_timing_params_and_request() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // ADR-072: ComParam-space timing values are microseconds; see the
    // ISO9141 counterpart test above for the conversion arithmetic.
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::PARITY, 0),
            (j2534_0404::P1_MAX, 10_000),
            (j2534_0404::P3_MIN, 27_500),
            (j2534_0404::W1, 60_000),
            (j2534_0404::W2, 20_000),
            (j2534_0404::W3, 55_000),
            (j2534_0404::W4, 5_000),
            (j2534_0404::W5, 300_000),
            (j2534_0404::TIDLE, 300_000),
            (j2534_0404::TINIL, 25_000),
            (j2534_0404::TWUP, 300_000),
        ],
    )
    .await;

    assert_eq!(server.backdoor.baud_rate(MOCK_CHANNEL_ID), 10_400);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::PARITY),
        0
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P1_MAX),
        20
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P3_MIN),
        55
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W1),
        60
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W2),
        20
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W3),
        55
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W4),
        5
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W5),
        300
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TIDLE),
        300
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TINIL),
        25
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TWUP),
        300
    );
    // W0 is ISO9141-only; ISO14230's connect must not forward it.
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W0),
        0
    );

    // ADR-050: cop_data is payload-only; the service constructs a 4-byte
    // KWP2000 header (format/target/source/length) from ComParams. No
    // CP_PhysReqFormatPriorityType/CP_PhysReqTargetAddr/NODE_ADDRESS were
    // set above, so the header uses this service's documented fallback
    // defaults (0x80 physical / 0x10 target / 0xF1 tester source).
    let payload = vec![0x22, 0x33];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    let mut expected = vec![0x80, 0x10, 0xF1, payload.len() as u8];
    expected.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::ISO14230
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    server.shutdown().await;
}

// ── ADR-181: CP_W1-W4 Min/Max ComParam storage-key collision fix ────────────
//
// Before this fix, `names.rs::map_comparam_name_native` resolved BOTH
// members of each CP_W{1,2,3}{Max,Min} / CP_W4{Min,Max} pair to the SAME
// native `ComParamId` (the literal HashMap key in Working/Active's
// `unum32` map), so setting one silently overwrote the other's stored
// value regardless of call order. These tests set both members of a pair
// (via `ParamName`, the same route the D-PDU shortname collision actually
// went through) in both orders and confirm each reads back its own value.

#[tokio::test]
#[serial]
async fn cp_w1max_then_cp_w1min_do_not_collide() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO9141, 1).await;

    set_com_param_by_name(&mut client, cll_handle, "CP_W1Max", 60_000).await;
    set_com_param_by_name(&mut client, cll_handle, "CP_W1Min", 5_000).await;

    assert_eq!(
        get_com_param_by_name_unum32(&mut client, cll_handle, "CP_W1Max").await,
        60_000
    );
    assert_eq!(
        get_com_param_by_name_unum32(&mut client, cll_handle, "CP_W1Min").await,
        5_000
    );

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn cp_w1min_then_cp_w1max_do_not_collide() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO9141, 1).await;

    set_com_param_by_name(&mut client, cll_handle, "CP_W1Min", 5_000).await;
    set_com_param_by_name(&mut client, cll_handle, "CP_W1Max", 60_000).await;

    assert_eq!(
        get_com_param_by_name_unum32(&mut client, cll_handle, "CP_W1Min").await,
        5_000
    );
    assert_eq!(
        get_com_param_by_name_unum32(&mut client, cll_handle, "CP_W1Max").await,
        60_000
    );

    server.shutdown().await;
}

/// Same collision fix, the opposite Min/Max-native-target asymmetry: for
/// `W4`, `CP_W4Min` is the side with the native register (unlike W1-W3,
/// where the native register belongs to the Max side) and `CP_W4Max` is
/// the store-only, project-minted side.
#[tokio::test]
#[serial]
async fn cp_w4min_then_cp_w4max_do_not_collide() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO9141, 1).await;

    set_com_param_by_name(&mut client, cll_handle, "CP_W4Min", 25_000).await;
    set_com_param_by_name(&mut client, cll_handle, "CP_W4Max", 50_000).await;

    assert_eq!(
        get_com_param_by_name_unum32(&mut client, cll_handle, "CP_W4Min").await,
        25_000
    );
    assert_eq!(
        get_com_param_by_name_unum32(&mut client, cll_handle, "CP_W4Max").await,
        50_000
    );

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn cp_w4max_then_cp_w4min_do_not_collide() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO9141, 1).await;

    set_com_param_by_name(&mut client, cll_handle, "CP_W4Max", 50_000).await;
    set_com_param_by_name(&mut client, cll_handle, "CP_W4Min", 25_000).await;

    assert_eq!(
        get_com_param_by_name_unum32(&mut client, cll_handle, "CP_W4Max").await,
        50_000
    );
    assert_eq!(
        get_com_param_by_name_unum32(&mut client, cll_handle, "CP_W4Min").await,
        25_000
    );

    server.shutdown().await;
}

/// ADR-181 also fixed a second, related bug: `comparam_defaults.rs`'s
/// native `W4` slot had been seeded with `CP_W4Max`'s 50_000us default
/// instead of `CP_W4Min`'s own 25_000us default -- ISO 22900-2:2009(E)
/// Table A.3 makes the native W4 register CP_W4Min's, not CP_W4Max's, so
/// the wrong ComParam's default was silently reaching hardware. This is an
/// intended, wire-visible behavior change: a fresh K-line preset with no
/// ComParam overrides now forwards a native `W4` of 25 (ms) instead of the
/// old, incorrect 50. Resource `0x0213` (`ISO_OBD_on_K_Line`) is the same
/// no-override preset `resource_0213_default_path_selects_fast_init_with_no_overrides`
/// (`startcomm_comparam.rs`) uses: it connects on ISO9141 via
/// `iso_obd_on_k_line`'s protocol defaults, which seed native `W4` through
/// `kwp_on_kline_common`.
#[tokio::test]
#[serial]
async fn resource_0213_fresh_preset_forwards_corrected_w4_min_default() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    create_and_connect_cll(&mut client, 0x0213, &[]).await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W4),
        25,
        "the native W4 slot is CP_W4Min's own register; a fresh preset's forwarded value \
         must reflect CP_W4Min's corrected 25_000us default (ADR-181), not CP_W4Max's old \
         (and wrong) 50_000us"
    );

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn j1850vpw_protocol_forwards_baud_rate_and_frame() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850VPW,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    assert_eq!(server.backdoor.baud_rate(MOCK_CHANNEL_ID), 10_400);

    // ADR-050: cop_data is payload-only; the service constructs the 3-byte
    // J1850 header (format/target/source) from ComParams. No
    // CP_PhysReqFormatPriorityType/CP_PhysReqTargetAddr/NODE_ADDRESS were
    // set above, so the header uses this service's documented fallback
    // defaults (0x68 = standard OBD-II VPW priority byte / 0x10 target /
    // 0xF1 tester source).
    let payload = vec![0x01, 0x00];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    let mut expected = vec![0x68, 0x10, 0xF1];
    expected.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::J1850VPW
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn j1850pwm_protocol_forwards_network_line_and_frame() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850PWM,
        &[
            (j2534_0404::DATA_RATE, 41_600),
            (j2534_0404::NETWORK_LINE, 0),
        ],
    )
    .await;

    assert_eq!(server.backdoor.baud_rate(MOCK_CHANNEL_ID), 41_600);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::NETWORK_LINE),
        0
    );

    // ADR-050: cop_data is payload-only; the service constructs the 3-byte
    // J1850 header (format/target/source) from ComParams. No
    // CP_PhysReqFormatPriorityType/CP_PhysReqTargetAddr/NODE_ADDRESS were
    // set above, so the header uses this service's documented fallback
    // defaults (0x61 = standard OBD-II PWM priority byte / 0x10 target /
    // 0xF1 tester source).
    let payload = vec![0x01, 0x00];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    let mut expected = vec![0x61, 0x10, 0xF1];
    expected.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::J1850PWM
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    server.shutdown().await;
}

// ── J1850 addressing ComParam overrides (backlog fix) ───────────────────────
//
// `is_j1850pwm_param`/`is_j1850vpw_param` previously omitted
// `CP_PhysReqFormatPriorityType`/`CP_PhysReqTargetAddr`/
// `CP_FuncReqFormatPriorityType`/`CP_FuncReqTargetAddr`/`NODE_ADDRESS` from
// their `SetComParam` allow-lists, even though `tx_header::
// j1850_header_bytes` reads all five (ADR-050/ADR-054) -- a client could
// never override them on a bare-protocol J1850 CLL and was stuck with the
// fallback defaults the two tests above exercise
// (`j1850vpw_protocol_forwards_baud_rate_and_frame`'s 0x68/0x10/0xF1,
// `j1850pwm_protocol_forwards_network_line_and_frame`'s 0x61/0x10/0xF1).
// These two tests confirm `SetComParam`/`GetComParam` now succeed on these
// IDs for both J1850 variants, and that the overridden -- not the fallback
// -- bytes are what actually reaches the wire.

#[tokio::test]
#[serial]
async fn j1850vpw_set_com_param_overrides_physical_addressing_on_wire() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850VPW,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    set_com_param_unum32(&mut client, cll_handle, CP_PHYS_REQ_FORMAT_PRIORITY, 0x6C).await;
    set_com_param_unum32(&mut client, cll_handle, CP_PHYS_REQ_TARGET_ADDR, 0x33).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::NODE_ADDRESS, 0xF2).await;

    // GetComParam always reads Working -- confirms the staged overrides took,
    // independent of the CoptUpdateparam promotion exercised below.
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_PHYS_REQ_FORMAT_PRIORITY).await,
        0x6C,
        "GetComParam should read back the staged CP_PhysReqFormatPriorityType override"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_PHYS_REQ_TARGET_ADDR).await,
        0x33,
        "GetComParam should read back the staged CP_PhysReqTargetAddr override"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::NODE_ADDRESS).await,
        0xF2,
        "GetComParam should read back the staged NODE_ADDRESS override"
    );

    // Promote Working -> Active (ADR-067) so the override is visible to the
    // CoptSendrecv below.
    promote_via_update_param(&mut client, cll_handle).await;

    let payload = vec![0x01, 0x00];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    let mut expected = vec![0x6C, 0x33, 0xF2];
    expected.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected,
        "the overridden CP_PhysReqFormatPriorityType/CP_PhysReqTargetAddr/NODE_ADDRESS values \
         should reach the wire, not the 0x68/0x10/0xF1 fallback defaults"
    );

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn j1850pwm_set_com_param_overrides_functional_addressing_on_wire() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850PWM,
        &[(j2534_0404::DATA_RATE, 41_600)],
    )
    .await;

    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_FUNC_REQ_FORMAT_PRIORITY, 0xC8).await;
    set_com_param_unum32(&mut client, cll_handle, CP_FUNC_REQ_TARGET_ADDR, 0xAA).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::NODE_ADDRESS, 0xF3).await;

    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_FUNC_REQ_FORMAT_PRIORITY).await,
        0xC8,
        "GetComParam should read back the staged CP_FuncReqFormatPriorityType override"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_FUNC_REQ_TARGET_ADDR).await,
        0xAA,
        "GetComParam should read back the staged CP_FuncReqTargetAddr override"
    );

    promote_via_update_param(&mut client, cll_handle).await;

    let payload = vec![0x01, 0x00];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    let mut expected = vec![0xC8, 0xAA, 0xF3];
    expected.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected,
        "CP_RequestAddrMode = 2 should select the overridden \
         CP_FuncReqFormatPriorityType/CP_FuncReqTargetAddr/NODE_ADDRESS on the wire, not the \
         0x61/0x10/0xF1 physical fallback defaults"
    );

    server.shutdown().await;
}

// ── CP_UartConfig decode (ADR-071) ──────────────────────────────────────────

/// `CP_UartConfig` (ComParam ID `0x20`, numerically overlapping the native
/// `DATA_BITS` config ID -- ADR-027) is decoded, not forwarded raw: value `8`
/// (8E1) must reach the hardware as `DATA_BITS = 0` (8 data bits) AND
/// `PARITY = 2` (even), split from the single ComParam value.
#[tokio::test]
#[serial]
async fn iso9141_uart_config_decodes_to_data_bits_and_parity() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::DATA_BITS, 8), // CP_UartConfig = 8 (8E1)
        ],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::DATA_BITS),
        0
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::PARITY),
        2
    );

    server.shutdown().await;
}

/// When both `CP_UartConfig` (`0x20`) and the explicit J2534-specific
/// `CP_Parity` alias (`0x16`) are set, the explicit `CP_Parity` value wins --
/// deterministic regardless of `HashMap` iteration order (ADR-071).
#[tokio::test]
#[serial]
async fn iso9141_explicit_parity_overrides_uart_config_derived_parity() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::DATA_BITS, 8), // CP_UartConfig = 8 (8E1 -> PARITY = 2)
            (j2534_0404::PARITY, 1),    // explicit CP_Parity = odd
        ],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::DATA_BITS),
        0
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::PARITY),
        1
    );

    server.shutdown().await;
}

/// `SetComParam` rejects `CP_UartConfig` values J2534 v04.04 cannot
/// represent (2-stop-bit and 9-data-bit encodings) and out-of-range
/// `CP_Parity` values, with `INVALID_ARGUMENT` (ADR-071).
#[tokio::test]
#[serial]
async fn set_com_param_rejects_unrepresentable_uart_config_and_parity() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO9141, 1).await;

    // CP_UartConfig = 9: 8 data bits, 2 stop bits -- no J2534 stop-bit param.
    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    j2534_0404::DATA_BITS,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(9)),
            }),
        })
        .await
        .expect_err("SetComParam(CP_UartConfig=9) should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    // CP_UartConfig = 12: 9 data bits -- DATA_BITS only encodes 7/8.
    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    j2534_0404::DATA_BITS,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(12)),
            }),
        })
        .await
        .expect_err("SetComParam(CP_UartConfig=12) should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    // CP_Parity = 3: out of the native PARITY 0..=2 range.
    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    j2534_0404::PARITY,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(3)),
            }),
        })
        .await
        .expect_err("SetComParam(CP_Parity=3) should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// `SetComParam` rejects `CP_BlockSizeOverride` (direct-reuse mapping onto
/// native `BS_TX`, `names.rs`) values that ISO 22900-2's Table B.14 allows
/// but SAE J2534-1 Figure 30's native `BS_TX` range does not: J2534-1
/// restricts `BS_TX` to `0x00..=0xFF` plus the `0xFFFF` sentinel, so a value
/// in `0x100..0xFFFE` must be rejected with `INVALID_ARGUMENT` here, rather
/// than reaching `apply_params_to_hardware_locked`'s batched `SET_CONFIG`
/// and silently blocking every other staged ComParam in the same batch.
#[tokio::test]
#[serial]
async fn set_com_param_rejects_out_of_range_block_size_override() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;

    // CP_BlockSizeOverride = 0x100: inside the invalid 0x100..0xFFFE gap.
    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    j2534_0404::BS_TX,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(0x100)),
            }),
        })
        .await
        .expect_err("SetComParam(CP_BlockSizeOverride=0x100) should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// `SetComParam` accepts `CP_BlockSizeOverride` values in `0x100..0xFFFE` on
/// a plain `CAN` link. `CP_BlockSizeOverride` is a shared ComParamId allowed
/// on both `CAN` and `ISO15765` links, but `to_j2534_config_id` only maps it
/// onto native `BS_TX` -- and thus only actually risks the batched
/// `SET_CONFIG` failure the range check above guards against -- on an
/// `ISO15765` link; on `CAN` it is simply stored service-side and never
/// forwarded to hardware, so the full ISO 22900-2 Table B.14 `[0, 0xFFFF]`
/// range is legitimate here and must not be rejected.
#[tokio::test]
#[serial]
async fn set_com_param_accepts_out_of_range_block_size_override_on_can() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;

    // CP_BlockSizeOverride = 0x100: inside the 0x100..0xFFFE gap that is
    // rejected on ISO15765, but never reaches native BS_TX on plain CAN.
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    j2534_0404::BS_TX,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(0x100)),
            }),
        })
        .await
        .expect("SetComParam(CP_BlockSizeOverride=0x100) on CAN should be accepted");

    server.shutdown().await;
}

/// `SetComParam(CP_BlockSizeOverride, 0)` (the low boundary of the valid
/// native `BS_TX` byte range) is accepted and forwarded unchanged.
#[tokio::test]
#[serial]
async fn set_com_param_accepts_block_size_override_low_boundary() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000), (j2534_0404::BS_TX, 0)],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BS_TX),
        0
    );

    server.shutdown().await;
}

/// `SetComParam(CP_BlockSizeOverride, 0xFF)` (the high boundary of the valid
/// native `BS_TX` byte range) is accepted and forwarded unchanged.
#[tokio::test]
#[serial]
async fn set_com_param_accepts_block_size_override_high_boundary() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000), (j2534_0404::BS_TX, 0xFF)],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BS_TX),
        0xFF
    );

    server.shutdown().await;
}

/// `SetComParam(CP_BlockSizeOverride, 0xFFFF)` (the "use the
/// vehicle-reported flow-control value" sentinel) is accepted and forwarded
/// unchanged.
#[tokio::test]
#[serial]
async fn set_com_param_accepts_block_size_override_sentinel() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::BS_TX, 0xFFFF),
        ],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BS_TX),
        0xFFFF
    );

    server.shutdown().await;
}

/// `SetComParam(0x20, 0)` (7N1, the low boundary of the accepted set) is
/// accepted and decodes to `DATA_BITS = 1` (7 data bits) / `PARITY = 0`
/// (none) -- the counterpart of `iso9141_uart_config_decodes_to_data_bits_and_parity`,
/// which covers the high boundary (`0x20 = 8`, 8E1).
#[tokio::test]
#[serial]
async fn iso9141_uart_config_low_boundary_7n1_accepted() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::DATA_BITS, 0), // CP_UartConfig = 0 (7N1)
        ],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::DATA_BITS),
        1
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::PARITY),
        0
    );

    server.shutdown().await;
}

/// `CP_UartConfig` is accepted by the SCI allowlist and range-checked the
/// same way as on ISO9141/ISO14230, but SCI has no J2534 `SET_CONFIG`
/// support for either `DATA_BITS` or `PARITY` (`to_j2534_config_id` returns
/// `None` for SCI protocols) -- the value is stored in the Working ComParam
/// set only and never reaches the hardware adapter.
#[tokio::test]
#[serial]
async fn sci_uart_config_accepted_but_not_forwarded_to_hardware() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::SCI_A_ENGINE,
        &[
            (j2534_0404::DATA_RATE, 7_812),
            (j2534_0404::DATA_BITS, 8), // CP_UartConfig = 8 (8E1)
        ],
    )
    .await;

    // Neither DATA_BITS nor PARITY was ever forwarded via SET_CONFIG, so the
    // mock's per-channel config store still reports the unset default (0)
    // for both -- not the decoded values a KWP channel would show.
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::DATA_BITS),
        0
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::PARITY),
        0
    );

    server.shutdown().await;
}

/// Save/restore roundtrip idempotence (follow-up to ADR-071): a client that
/// reads back `CP_UartConfig` (`0x20`) and `CP_Parity` (`0x16`) via
/// `GetComParam` and later restores both via `SetComParam` must not corrupt
/// the effective parity. Before this fix, `GetComParam(0x16)` returned a
/// stale `0` when no explicit `0x16` entry existed in Working, which the
/// restore step would then write back as an explicit (and wrong) `CP_Parity
/// = 0`, silently downgrading 8E1 to 8N1. `GetComParam(0x16)` now derives
/// the value from the Working `CP_UartConfig` entry when no explicit `0x16`
/// entry exists, so the restored explicit value matches what was already
/// effectively in force -- forwarding to hardware is unchanged by the
/// roundtrip.
#[tokio::test]
#[serial]
async fn uart_config_parity_get_set_roundtrip_is_idempotent() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::DATA_BITS, 8), // CP_UartConfig = 8 (8E1)
        ],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::DATA_BITS),
        0
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::PARITY),
        2
    );

    // "Save": GetComParam(0x16) must report the *effective* parity (2, even)
    // derived from the Working CP_UartConfig entry, not a stale 0 -- there
    // was never an explicit SetComParam(0x16, ...) on this CLL.
    let saved_parity = get_com_param_unum32(&mut client, cll_handle, j2534_0404::PARITY).await;
    assert_eq!(
        saved_parity, 2,
        "GetComParam(CP_Parity) should derive from CP_UartConfig"
    );

    // "Restore": write the saved value back explicitly, then push Working to
    // hardware via CoptUpdateparam.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::PARITY, saved_parity).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(
                vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_handle),
            ),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptUpdateparam) should succeed");
    wait_for_cop_finished(&mut events).await;

    // Idempotent: hardware still shows 8E1 (DATA_BITS = 0, PARITY = 2), not
    // the corrupted 8N1 a stale-0 restore would have produced.
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::DATA_BITS),
        0
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::PARITY),
        2
    );

    drop(events);
    server.shutdown().await;
}

/// The `CoptUpdateparam` hardware-forwarding path (`events.rs::apply_params_to_hardware`)
/// applies the same `CP_UartConfig` -> `DATA_BITS`+`PARITY` split as the
/// `ConnectComLogicalLink` path (`rpc_link.rs::apply_j2534_params`): staging
/// a new `CP_UartConfig` value in Working and promoting it via
/// `CoptUpdateparam` must push both native entries, not just `DATA_BITS`.
#[tokio::test]
#[serial]
async fn iso9141_updateparam_splits_uart_config_into_data_bits_and_parity() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    // Connect-time default (comparam_defaults.rs): CP_UartConfig = 6 (8N1).
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::DATA_BITS),
        0
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::PARITY),
        0
    );

    // Stage CP_UartConfig = 8 (8E1) in Working; not yet promoted.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_BITS, 8).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(
                vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_handle),
            ),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptUpdateparam) should succeed");
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::DATA_BITS),
        0
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::PARITY),
        2
    );

    drop(events);
    server.shutdown().await;
}

// ── ISO 22900 <-> J2534 timing ComParam unit conversion (ADR-072) ──────────

/// `P1_MAX`/`P3_MIN`/`P4_MIN` convert from ComParam-space microseconds to
/// native J2534 0.5 ms steps, and `W1`/`TINIL`/`TWUP` convert to native 1 ms
/// steps, via `to_j2534_config_value` (ADR-072).
#[tokio::test]
#[serial]
async fn iso14230_timing_params_convert_from_microseconds_to_native_units() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::P1_MAX, 20_000),
            (j2534_0404::P3_MIN, 55_000),
            (j2534_0404::P4_MIN, 5_000),
            (j2534_0404::W1, 300_000),
            (j2534_0404::TINIL, 25_000),
            (j2534_0404::TWUP, 50_000),
        ],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P1_MAX),
        40
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P3_MIN),
        110
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P4_MIN),
        10
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W1),
        300
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TINIL),
        25
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TWUP),
        50
    );

    server.shutdown().await;
}

/// `CP_TIdle` fans out to `W5` on an ISO14230 link (`expand_tidle`,
/// ADR-072): a single `SetComParam(TIDLE, ...)` reaches both native `TIDLE`
/// and `W5` `SET_CONFIG` entries, unit-converted the same way.
#[tokio::test]
#[serial]
async fn iso14230_tidle_fans_out_to_w5() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::TIDLE, 300_000),
        ],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TIDLE),
        300
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W5),
        300
    );

    server.shutdown().await;
}

/// `CP_TIdle` fans out to `W0` on an ISO9141 link (`expand_tidle`,
/// ADR-072): the ISO9141 counterpart of `iso14230_tidle_fans_out_to_w5`.
#[tokio::test]
#[serial]
async fn iso9141_tidle_fans_out_to_w0() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::TIDLE, 300_000),
        ],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TIDLE),
        300
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W0),
        300
    );

    server.shutdown().await;
}

/// An explicit `SetComParam(W5, ...)` takes precedence over the value
/// `CP_TIdle` would otherwise derive for `W5` (`expand_tidle`'s explicit-
/// entry-wins rule, ADR-072, mirroring `expand_uart_config`'s explicit-
/// `CP_Parity`-wins rule, ADR-071) -- `TIDLE` itself is unaffected. This
/// case sets `CP_TIdle` first, then the explicit `W5` -- see
/// `iso14230_explicit_w5_overrides_tidle_derived_value_set_before_tidle`
/// for the reverse order, which is the precedence rule's actual regression
/// case.
#[tokio::test]
#[serial]
async fn iso14230_explicit_w5_overrides_tidle_derived_value() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::TIDLE, 300_000),
            (j2534_0404::W5, 400_000),
        ],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W5),
        400
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TIDLE),
        300
    );

    server.shutdown().await;
}

/// Regression test: the explicit-entry-wins precedence must be
/// order-independent. Setting the explicit `W5` value BEFORE `CP_TIdle`
/// (the reverse of `iso14230_explicit_w5_overrides_tidle_derived_value`)
/// must produce the same result -- the explicit `W5` value must still win.
///
/// This is the scenario a set-time fan-out (an earlier draft of ADR-072's
/// implementation inserted a `CP_TIdle` value into the `W0`/`W5` Working
/// entries directly at `SetComParam` time, not just at forwarding time)
/// gets wrong: a later `SetComParam(CP_TIdle, ...)` would silently
/// overwrite the earlier explicit `W5` Working entry, and `expand_tidle`
/// would then see the overwritten entry as "explicit" and forward the
/// `CP_TIdle`-derived value instead. The only fan-out mechanism is
/// `expand_tidle`, evaluated fresh at the hardware-forwarding call sites
/// against the Working/Active snapshot -- `SetComParam(CP_TIdle, ...)`
/// never mutates `W0`/`W5`'s stored Working entries (ADR-072).
#[tokio::test]
#[serial]
async fn iso14230_explicit_w5_overrides_tidle_derived_value_set_before_tidle() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::W5, 400_000),
            (j2534_0404::TIDLE, 300_000),
        ],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W5),
        400
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TIDLE),
        300
    );

    server.shutdown().await;
}

/// Mirrors `iso14230_explicit_w5_overrides_tidle_derived_value_set_before_tidle`
/// for the ISO9141/`W0` pairing.
#[tokio::test]
#[serial]
async fn iso9141_explicit_w0_overrides_tidle_derived_value_set_before_tidle() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::W0, 400_000),
            (j2534_0404::TIDLE, 300_000),
        ],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::W0),
        400
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::TIDLE),
        300
    );

    server.shutdown().await;
}

/// `T1_MAX`-`T5_MAX` (SCI hardware timers) convert from ComParam-space
/// microseconds to native J2534 1 ms steps the same way as the KWP/K-line
/// timers above (ADR-072).
#[tokio::test]
#[serial]
async fn sci_timing_params_convert_from_microseconds_to_native_units() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::SCI_A_ENGINE,
        &[
            (j2534_0404::DATA_RATE, 7_812),
            (j2534_0404::T1_MAX, 20_000),
            (j2534_0404::T2_MAX, 100_000),
            (j2534_0404::T3_MAX, 50_000),
            (j2534_0404::T4_MAX, 20_000),
            (j2534_0404::T5_MAX, 100_000),
        ],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::T1_MAX),
        20
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::T2_MAX),
        100
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::T3_MAX),
        50
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::T4_MAX),
        20
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::T5_MAX),
        100
    );

    server.shutdown().await;
}

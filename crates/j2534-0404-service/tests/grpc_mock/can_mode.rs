//! `can_channel_mode` behaviours (ADR-046): `software-isotp` (the service
//! segments/reassembles ISO-TP itself on a raw CAN channel, including
//! extended addressing per the ADR-046 addendum), `dual-channel` (a
//! companion raw CAN channel serves UUDT responses), and `auto` (probe for
//! dual-channel capability, falling back to single-channel).

use serial_test::serial;
use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, DisconnectComLogicalLinkRequest,
    EventNotification, StartComPrimitiveRequest, SubscribeEventRequest, TxFlagBit, event_item,
    subscribe_event_request, vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// Subscribes to `cll_handle`'s events, mirroring `cop_ctrl_cycles.rs`'s own
/// `subscribe` helper (not shared via `harness.rs`, so duplicated here).
async fn subscribe(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
) -> tonic::Streaming<EventNotification> {
    client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner()
}

/// software-isotp mode: an ISO15765 CLL connects a raw CAN channel (verified
/// via the PASS_FILTER that only non-ISO15765 channels receive) and a short
/// request goes out as one ISO-TP SingleFrame built by the service.
#[tokio::test]
#[serial]
async fn software_isotp_mode_sends_single_frame_on_raw_can_channel() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // Raw CAN channel: pass-all PASS_FILTERs, not a FLOW_CONTROL_FILTER. Every
    // raw-CAN primary now always connects CAN_ID_BOTH (ADR-065), so it gets
    // one PASS_FILTER per CAN-ID type: TxFlags 0 (11-bit) and TX_EXTENDED_ID
    // (29-bit).
    assert_eq!(server.backdoor.connect_flags_log(), vec![0x800]);
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 0),
        0
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 1),
        j2534_0404::TX_EXTENDED_ID
    );

    // ADR-050: cop_data is payload-only; the service prepends the CAN ID
    // from the UniqueRespIdTable entry's CP_CanPhysReqId.
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    send_data(
        &mut client,
        cll_handle,
        vec![0x10, 0x03],
        vec![TxFlagBit::TxFlagIso15765FramePad],
    )
    .await;

    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::CAN
    );
    // Software padding: SingleFrame PCI + payload padded to 8 CAN data bytes.
    let mut expected = 0x7E0_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&[0x02, 0x10, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00]);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);
    // The ISO15765-only padding TxFlag must not reach the raw CAN channel.
    assert_eq!(server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0), 0);

    server.shutdown().await;
}

/// software-isotp mode: a 20-byte request is segmented into
/// FirstFrame + 2 ConsecutiveFrames, with the ConsecutiveFrames held back
/// until the ECU's FlowControl (ContinueToSend, BS=0, STmin=0) arrives.
#[tokio::test]
#[serial]
async fn software_isotp_mode_segments_multi_frame_request_with_flow_control() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // ADR-050: cop_data is payload-only; the service prepends the CAN ID
    // from the UniqueRespIdTable entry's CP_CanPhysReqId.
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let payload: Vec<u8> = (1..=20).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload.clone(),
            // NumSendCycles must be explicit (ADR-059): 0/unset now means no
            // send at all, not "send once."
            cop_ctrl_data: Some(vci_service_interface::ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    // FirstFrame goes out immediately; ConsecutiveFrames wait for FlowControl.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let mut expected_ff = 0x7E0_u32.to_be_bytes().to_vec();
    expected_ff.extend_from_slice(&[0x10, 0x14]);
    expected_ff.extend_from_slice(&payload[..6]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_ff
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    // ECU answers with FlowControl: ContinueToSend, BS=0 (all), STmin=0.
    let mut fc = 0x7E8_u32.to_be_bytes().to_vec();
    fc.extend_from_slice(&[0x30, 0x00, 0x00]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &fc, j2534_0404::CAN);

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let mut expected_cf1 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf1.push(0x21);
    expected_cf1.extend_from_slice(&payload[6..13]);
    let mut expected_cf2 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf2.push(0x22);
    expected_cf2.extend_from_slice(&payload[13..20]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_cf1
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 2),
        expected_cf2
    );

    server.shutdown().await;
}

/// ADR-098: a CONFIG_LOOPBACK echo of our own transmitted FlowControl frame
/// (`RxStatus` bit 0 = `TX_MSG_TYPE` set) must never be mistaken by
/// `FcCapture` for the ECU's genuine FlowControl -- the ConsecutiveFrames
/// stay withheld until a non-loopback-tagged FC arrives. Before ADR-098,
/// `FcCapture` had no `RX_TX_MSG_TYPE` guard at all, so this loopback-tagged
/// frame (which happens to carry the exact FlowControl byte pattern the TX
/// driver is waiting for) would have been wrongly captured and released the
/// ConsecutiveFrames early.
#[tokio::test]
#[serial]
async fn software_isotp_mode_loopback_echo_of_own_flow_control_frame_is_not_captured() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // ADR-050: cop_data is payload-only; the service prepends the CAN ID
    // from the UniqueRespIdTable entry's CP_CanPhysReqId.
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let payload: Vec<u8> = (1..=20).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload.clone(),
            cop_ctrl_data: Some(vci_service_interface::ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    // FirstFrame goes out immediately; ConsecutiveFrames wait for FlowControl.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A loopback-tagged echo carrying the exact FlowControl byte pattern the
    // TX driver is waiting for -- must NOT be captured.
    let mut loopback_fc = 0x7E8_u32.to_be_bytes().to_vec();
    loopback_fc.extend_from_slice(&[0x30, 0x00, 0x00]);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &loopback_fc,
        j2534_0404::CAN,
        0x0000_0001, // TX_MSG_TYPE (loopback echo)
    );

    // Give the poll task a chance to process the injected frame; the
    // ConsecutiveFrames must still be withheld.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a loopback-tagged FlowControl echo must not release the withheld ConsecutiveFrames"
    );

    // The ECU's genuine (non-loopback) FlowControl now arrives and unblocks
    // the ConsecutiveFrames normally.
    let mut fc = 0x7E8_u32.to_be_bytes().to_vec();
    fc.extend_from_slice(&[0x30, 0x00, 0x00]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &fc, j2534_0404::CAN);

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let mut expected_cf1 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf1.push(0x21);
    expected_cf1.extend_from_slice(&payload[6..13]);
    let mut expected_cf2 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf2.push(0x22);
    expected_cf2.extend_from_slice(&payload[13..20]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_cf1
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 2),
        expected_cf2
    );

    server.shutdown().await;
}

/// software-isotp mode: a segmented response (FF + CF) from the configured
/// USDT response ID is answered with a service-built FlowControl frame to the
/// paired CP_CanPhysReqId, reassembled, and delivered as one message.
#[tokio::test]
#[serial]
async fn software_isotp_mode_reassembles_segmented_response_and_sends_flow_control() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
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
            7,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // No hardware FLOW_CONTROL_FILTERs on a raw CAN channel: still just the
    // PASS_FILTERs from connect time -- two of them, since every raw-CAN
    // primary now always connects CAN_ID_BOTH (ADR-065).
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        j2534_0404::PASS_FILTER
    );

    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the reassembled
    // response below delivered under the new unbound-discard model; this
    // test's own table entry already has CP_CanPhysReqId set, so it resolves
    // directly (see `arm_receive_only_monitor`'s own doc comment).
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let response_payload: Vec<u8> = (0x60..0x60 + 10).collect();

    // ECU sends FirstFrame (10 bytes total, first 6 carried).
    let mut ff = 0x7E8_u32.to_be_bytes().to_vec();
    ff.extend_from_slice(&[0x10, 0x0A]);
    ff.extend_from_slice(&response_payload[..6]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &ff, j2534_0404::CAN);

    // The service must answer with FlowControl (ContinueToSend) to 0x7E0.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let mut expected_fc = 0x7E0_u32.to_be_bytes().to_vec();
    expected_fc.extend_from_slice(&[0x30, 0x00, 0x00]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_fc
    );

    // ECU sends the ConsecutiveFrame completing the message.
    let mut cf = 0x7E8_u32.to_be_bytes().to_vec();
    cf.push(0x21);
    cf.extend_from_slice(&response_payload[6..10]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &cf, j2534_0404::CAN);

    // The CLL receives one reassembled message; ADR-051 splits the CAN ID
    // into extra_info, leaving data_bytes as the reassembled payload.
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x7E8_u32.to_be_bytes(), &[], &response_payload);
    assert_eq!(result.unique_resp_identifier, 7);

    server.shutdown().await;
}

/// ADR-086 amendment (PR #92 review): `LogicalLinkState.isotp_rx` (the
/// per-CAN-ID software-ISO-TP reassembly map) must be cleared every time
/// `finalize_connected_link` stamps a fresh `connect_generation` -- including
/// a reconnect of the SAME `cll_handle` onto the SAME shared physical
/// channel -- otherwise a partial reassembly seeded by a FirstFrame from
/// BEFORE the reconnect can be completed by an unrelated ConsecutiveFrame
/// arriving AFTER the reconnect, splicing bytes from two different sessions
/// into one corrupted `ResultData`.
///
/// Reuses `software_isotp_mode_reassembles_segmented_response_and_sends_flow_control`'s
/// FirstFrame/ConsecutiveFrame injection technique and
/// `stopcomm_disconnect_then_reconnect_same_channel_suppresses_stale_final_transmit`'s
/// keep-a-second-CLL-alive reconnect technique (so cll_a's disconnect does
/// not tear the shared physical channel down, and its reconnect lands on the
/// identical `ChannelId` with a freshly-bumped `connect_generation`).
#[tokio::test]
#[serial]
async fn software_isotp_reconnect_clears_stale_reassembly_state() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // Kept alive (not disconnected) so the shared physical raw-CAN channel --
    // and its poll task -- survives cll_a's disconnect/reconnect below.
    let _cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical raw-CAN channel"
    );

    set_unique_resp_table_and_promote(
        &mut client,
        cll_a,
        vec![ecu_entry(
            7,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // Distinctive byte range so a corrupted/spliced payload is unmistakable.
    let stale_payload: Vec<u8> = (0xE0..0xEA).collect();

    // Seed a partial reassembly: FirstFrame only, no completing
    // ConsecutiveFrame -- the reassembly is left in-progress in
    // cll_a's `isotp_rx` map.
    let mut stale_ff = 0x7E8_u32.to_be_bytes().to_vec();
    stale_ff.extend_from_slice(&[0x10, 0x0A]);
    stale_ff.extend_from_slice(&stale_payload[..6]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &stale_ff, j2534_0404::CAN);

    // The service answers the FirstFrame with FlowControl -- confirms the
    // partial reassembly was actually seeded before the reconnect.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Disconnect and immediately reconnect the SAME cll_handle, at the SAME
    // DATA_RATE, rejoining the SAME shared physical channel (kept alive by
    // cll_b) with the identical ChannelId but a freshly-bumped
    // connect_generation (ADR-086).
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("reconnecting cll_a on the same shared channel should succeed");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "the reconnect must rejoin the existing shared channel, not open a new one"
    );
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the fresh
    // reassembled response (after the reconnect, below) delivered under the
    // new unbound-discard model -- registered fresh here (not before the
    // reconnect) since a pre-reconnect registrant would itself be
    // generation-stale and excluded from attribution (ADR-086).
    arm_receive_only_monitor(&mut client, cll_a).await;

    // A ConsecutiveFrame on the SAME CAN ID, with the sequence number the
    // stale FirstFrame is expecting next -- exactly what would complete the
    // STALE reassembly if it survived the reconnect. This represents a frame
    // from the NEW session (arriving after reconnect), unrelated to the
    // stale FirstFrame.
    let new_session_cf_payload = [0xDE, 0xAD, 0xBE, 0xEF];
    let mut new_session_cf = 0x7E8_u32.to_be_bytes().to_vec();
    new_session_cf.push(0x21);
    new_session_cf.extend_from_slice(&new_session_cf_payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &new_session_cf, j2534_0404::CAN);

    // With the stale reassembly cleared at reconnect, this is a stray CF with
    // no in-progress reassembly for its CAN ID: no ResultData (correct,
    // uncorrupted) and no reply frame is produced.
    assert_no_result_data(
        &mut client,
        cll_a,
        "a ConsecutiveFrame after reconnect must not complete a pre-reconnect FirstFrame",
    )
    .await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the stray post-reconnect ConsecutiveFrame must not provoke a FlowControl reply"
    );

    // A genuinely fresh, complete FirstFrame + ConsecutiveFrame sequence
    // after the reconnect must still reassemble correctly, with no bytes
    // carried over from the stale pre-reconnect session or the stray CF
    // above.
    let fresh_payload: Vec<u8> = (0x10..0x1A).collect();
    let mut fresh_ff = 0x7E8_u32.to_be_bytes().to_vec();
    fresh_ff.extend_from_slice(&[0x10, 0x0A]);
    fresh_ff.extend_from_slice(&fresh_payload[..6]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &fresh_ff, j2534_0404::CAN);
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    let mut fresh_cf = 0x7E8_u32.to_be_bytes().to_vec();
    fresh_cf.push(0x21);
    fresh_cf.extend_from_slice(&fresh_payload[6..10]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &fresh_cf, j2534_0404::CAN);

    let result = wait_for_result_data(&mut client, cll_a).await;
    assert_result_data(&result, &0x7E8_u32.to_be_bytes(), &[], &fresh_payload);
    assert_eq!(result.unique_resp_identifier, 7);

    server.shutdown().await;
}

/// dual-channel mode: configuring a CP_CanRespUUDTId opens a companion raw
/// CAN channel (second PassThruConnect) instead of installing the ADR-041
/// UUDT FLOW_CONTROL_FILTER on the ISO15765 channel, and UUDT frames arriving
/// on the companion channel are delivered to the CLL.
#[tokio::test]
#[serial]
async fn dual_channel_mode_opens_companion_can_channel_for_uudt() {
    const COMPANION_CHANNEL_ID: u32 = 2;

    let server = TestServer::start_with_can_mode(Some("dual-channel")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // Companion CAN channel opened at the same baud rate. It always connects
    // CAN_ID_BOTH (0x800) since the CLL's UUDT response CAN-ID type is
    // per-entry and unknown/heterogeneous at connect time (ADR-065), so it
    // gets a pass-all PASS_FILTER for each ID type: TxFlags 0 (11-bit) and
    // TX_EXTENDED_ID (29-bit).
    assert_eq!(server.backdoor.connect_count(), 2);
    assert_eq!(server.backdoor.baud_rate(COMPANION_CHANNEL_ID), 500_000);
    assert_eq!(server.backdoor.connect_flags_log(), vec![0, 0x800]);
    assert_eq!(server.backdoor.filter_count(COMPANION_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.filter_type(COMPANION_CHANNEL_ID, 0),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server
            .backdoor
            .filter_pattern_tx_flags(COMPANION_CHANNEL_ID, 0),
        0
    );
    assert_eq!(
        server.backdoor.filter_type(COMPANION_CHANNEL_ID, 1),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server
            .backdoor
            .filter_pattern_tx_flags(COMPANION_CHANNEL_ID, 1),
        j2534_0404::TX_EXTENDED_ID
    );

    // The ISO15765 channel keeps only the USDT point-to-point filter — no
    // ADR-041 UUDT filter in dual-channel mode.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        0x7E8_u32.to_be_bytes()
    );

    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the UUDT frame
    // below delivered under the new unbound-discard model; this test's own
    // table entry already has CP_CanPhysReqId set, so it resolves directly.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // A UUDT frame on the companion channel reaches the CLL with its
    // unique_resp_identifier.
    let uudt_payload = vec![0x62, 0xF1, 0x90, 0xAA];
    let mut uudt = 0x5E8_u32.to_be_bytes().to_vec();
    uudt.extend_from_slice(&uudt_payload);
    server
        .backdoor
        .inject_rx(COMPANION_CHANNEL_ID, &uudt, j2534_0404::CAN);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x5E8_u32.to_be_bytes(), &[], &uudt_payload);
    assert_eq!(result.unique_resp_identifier, 3);

    // An unrelated frame on the companion channel (e.g. the raw view of USDT
    // traffic) is NOT delivered to this CLL.
    let unrelated = can_frame(0x7E8, &[0x02, 0x50, 0x03]);
    server
        .backdoor
        .inject_rx(COMPANION_CHANNEL_ID, &unrelated, j2534_0404::CAN);
    assert_no_result_data(
        &mut client,
        cll_handle,
        "companion-channel frame not matching a UUDT ID must be dropped",
    )
    .await;

    server.shutdown().await;
}

/// ADR-217 Decision item 5 (Codex review, PR #132): regression coverage for
/// a pre-existing exposure this same fix also closes, independent of
/// ADR-217's own `ALL_FRAMES` feature -- `dual-channel` mode is not gated by
/// ADR-162's `NativeMixed`-only collision check at all, so a row whose
/// `CP_CanRespUSDTId` and `CP_CanRespUUDTId` are the SAME CAN ID but
/// DIFFERENT `Addressing` has always been an accepted configuration here.
/// Before the role-split fix, `header_footer_len`'s flat `can_addressing_by_id`
/// table would see the USDT entry's `Addressing::Extended` and wrongly widen
/// the companion channel's own UUDT-routed (always normal-addressed here)
/// delivery to a 5-byte header, stripping its first real payload byte.
#[tokio::test]
#[serial]
async fn dual_channel_mode_companion_uudt_delivery_keeps_its_own_normal_addressing_despite_colliding_extended_usdt_entry()
 {
    const COMPANION_CHANNEL_ID: u32 = 2;

    let server = TestServer::start_with_can_mode(Some("dual-channel")).await;
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
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x5E8),
                unum32_param(CP_CAN_RESP_USDT_FORMAT, can_id_format::EXTENDED_11BIT),
                unum32_param(CP_CAN_RESP_USDT_EXT_ADDR, 0xF1),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0xAA, 0xBB, 0xCC, 0xDD];
    let frame = can_frame(0x5E8, &payload);
    server
        .backdoor
        .inject_rx(COMPANION_CHANNEL_ID, &frame, j2534_0404::CAN);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x5E8_u32.to_be_bytes(), &[], &payload);
    assert_eq!(result.unique_resp_identifier, 3);

    server.shutdown().await;
}

/// `can_channel_mode = "auto"`: when the device can open a companion CAN
/// channel alongside the ISO15765 channel, the probe resolves to
/// dual-channel behaviour — same companion-channel-opens-on-UUDT-ID
/// observable outcome as an explicit `"dual-channel"` config.
#[tokio::test]
#[serial]
async fn auto_can_channel_mode_resolves_to_dual_channel_when_capable() {
    const COMPANION_CHANNEL_ID: u32 = 2;

    let server = TestServer::start_with_can_mode(Some("auto")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // The probe itself opens and immediately releases a companion channel:
    // 2 PassThruConnect calls (primary + probe), 1 PassThruDisconnect
    // (probe release), leaving only the primary channel open.
    assert_eq!(server.backdoor.connect_count(), 2);
    assert!(server.backdoor.baud_rate(MOCK_CHANNEL_ID) == 500_000);

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // Companion channel re-opened for the actual UUDT ID configuration.
    assert_eq!(server.backdoor.connect_count(), 3);
    assert_eq!(server.backdoor.baud_rate(COMPANION_CHANNEL_ID + 1), 500_000);

    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the UUDT frame
    // below delivered under the new unbound-discard model; this test's own
    // table entry already has CP_CanPhysReqId set, so it resolves directly.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let uudt_payload = vec![0x62, 0xF1, 0x90, 0xAA];
    let mut uudt = 0x5E8_u32.to_be_bytes().to_vec();
    uudt.extend_from_slice(&uudt_payload);
    server
        .backdoor
        .inject_rx(COMPANION_CHANNEL_ID + 1, &uudt, j2534_0404::CAN);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x5E8_u32.to_be_bytes(), &[], &uudt_payload);
    assert_eq!(result.unique_resp_identifier, 3);

    server.shutdown().await;
}

/// `can_channel_mode = "auto"`: when the device cannot open a second
/// channel (simulated via `__mock_set_max_channels(1)`), the probe fails,
/// resolves to single-channel behaviour, and the CLL still works normally
/// on its one ISO15765 channel — no UUDT companion is attempted again.
#[tokio::test]
#[serial]
async fn auto_can_channel_mode_falls_back_to_single_channel_when_incapable() {
    let server = TestServer::start_with_can_mode(Some("auto")).await;
    server.backdoor.set_max_channels(1);
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // Probe attempted and failed with ERR_EXCEEDED_LIMIT (the mock's connect
    // counter only counts successful PassThruConnect calls, so it stays at
    // 1 — just the primary channel); no companion channel is left open.
    // No UniqueRespIdTable configured yet, so no filter at Connect (ADR-048).
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);

    // Falls back to the pre-ADR-046 UUDT FLOW_CONTROL_FILTER workaround
    // instead of a companion channel.
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // No successful PassThruConnect beyond the primary channel; two
    // point-to-point filters (USDT + UUDT) installed on it instead.
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    server.shutdown().await;
}

/// software-isotp mode with extended addressing (ADR-046 addendum): a
/// UniqueRespIdTable entry configured with `CP_CanPhysReqFormat` /
/// `CP_CanRespUSDTFormat` bit 3 set gets a one-byte Address Extension on
/// every frame it builds or parses. Covers both directions in one CLL:
/// TX (multi-frame request, FC-paced) and RX (segmented response,
/// service-built FC reply) — each with its own AE byte
/// (`CP_CanPhysReqExtAddr` for our own frames, `CP_CanRespUSDTExtAddr` for
/// the ECU's).
#[tokio::test]
#[serial]
async fn software_isotp_mode_extended_addressing_segments_and_reassembles() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    const REQ_EXT_ADDR: u32 = 0x01;
    const RESP_EXT_ADDR: u32 = 0xF1;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            5,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                unum32_param(CP_CAN_PHYS_REQ_FORMAT, can_id_format::EXTENDED_11BIT),
                unum32_param(CP_CAN_PHYS_REQ_EXT_ADDR, REQ_EXT_ADDR),
                unum32_param(CP_CAN_RESP_USDT_FORMAT, can_id_format::EXTENDED_11BIT),
                unum32_param(CP_CAN_RESP_USDT_EXT_ADDR, RESP_EXT_ADDR),
            ],
        )],
    )
    .await;

    // ── TX: 20-byte request, extended-addressing FF (5-byte payload) + CFs (6-byte payload) ──
    // ADR-050: cop_data is payload-only; the service prepends the CAN ID
    // and AE byte from the UniqueRespIdTable entry configured above.
    let payload: Vec<u8> = (1..=20).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload.clone(),
            // NumSendCycles must be explicit (ADR-059): 0/unset now means no
            // send at all, not "send once."
            cop_ctrl_data: Some(vci_service_interface::ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let mut expected_ff = 0x7E0_u32.to_be_bytes().to_vec();
    expected_ff.push(REQ_EXT_ADDR as u8); // AE
    expected_ff.extend_from_slice(&[0x10, 0x14]); // FF PCI, total_len=20
    expected_ff.extend_from_slice(&payload[..5]); // 5-byte FF payload (extended)
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_ff
    );

    // ECU's FlowControl carries its own AE (CP_CanRespUSDTExtAddr).
    let mut fc = 0x7E8_u32.to_be_bytes().to_vec();
    fc.push(RESP_EXT_ADDR as u8);
    fc.extend_from_slice(&[0x30, 0x00, 0x00]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &fc, j2534_0404::CAN);

    // 15 remaining bytes / 6-byte extended-addressing CF capacity = 3 CFs (6, 6, 3).
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 4).await;
    let mut expected_cf1 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf1.push(REQ_EXT_ADDR as u8);
    expected_cf1.push(0x21);
    expected_cf1.extend_from_slice(&payload[5..11]); // 6-byte CF payload (extended)
    let mut expected_cf2 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf2.push(REQ_EXT_ADDR as u8);
    expected_cf2.push(0x22);
    expected_cf2.extend_from_slice(&payload[11..17]);
    let mut expected_cf3 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf3.push(REQ_EXT_ADDR as u8);
    expected_cf3.push(0x23);
    expected_cf3.extend_from_slice(&payload[17..20]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_cf1
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 2),
        expected_cf2
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 3),
        expected_cf3
    );

    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the reassembled
    // RX response below delivered under the new unbound-discard model (the
    // TX-side CoptSendrecv above never registers its own receive-phase
    // candidate -- NumReceiveCycles is unset/0, fire-and-forget). Armed only
    // now, after the client's own request has fully finished sending: a
    // created-receive-only (`NumSendCycles == 0`) `NumReceiveCycles == -1`
    // registrant's wait blocks this shared channel's poll task until its own
    // first match (a pre-existing S6 property, not freeing the poll task
    // immediately the way a migrated IS-CYCLIC registrant's post-first-match
    // detachment does, ADR-100 Decision §2) -- arming it earlier would wedge
    // the still-pending client request behind it.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // ── RX: 10-byte segmented response from the ECU, extended-addressing FF (5) + CF (5) ──
    let response_payload: Vec<u8> = (0x60..0x60 + 10).collect();

    let mut ff = 0x7E8_u32.to_be_bytes().to_vec();
    ff.push(RESP_EXT_ADDR as u8);
    ff.extend_from_slice(&[0x10, 0x0A]);
    ff.extend_from_slice(&response_payload[..5]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &ff, j2534_0404::CAN);

    // The service's FlowControl reply carries the request's AE (CP_CanPhysReqExtAddr).
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 5).await;
    let mut expected_fc = 0x7E0_u32.to_be_bytes().to_vec();
    expected_fc.push(REQ_EXT_ADDR as u8);
    expected_fc.extend_from_slice(&[0x30, 0x00, 0x00]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 4),
        expected_fc
    );

    let mut cf = 0x7E8_u32.to_be_bytes().to_vec();
    cf.push(RESP_EXT_ADDR as u8);
    cf.push(0x21);
    cf.extend_from_slice(&response_payload[5..10]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &cf, j2534_0404::CAN);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x7E8_u32.to_be_bytes(), &[], &response_payload);
    assert_eq!(result.unique_resp_identifier, 5);

    server.shutdown().await;
}

/// Backlog fix follow-up (edge-case-hunter, `ISO15765_ADDR_TYPE` protocol-gate
/// fix): under `can_channel_mode = "software-isotp"` (ADR-046), an ISO15765
/// CLL's actually-transmitted frames go out with `hw_protocol_id ==
/// j2534_0404::CAN` (`CanChannelMode::hw_protocol_id`), not `ISO15765`, even
/// though the CLL was connected with protocol ISO15765 -- the vendor DLL
/// performs no automatic ISO15765 processing on these raw-CAN writes, since
/// this service's own software driver handles all ISO-TP framing itself.
/// `tx_header::can_addressing_tx_flags`'s protocol gate therefore must never
/// set `ISO15765_ADDR_TYPE` on such a write, matching the same "never on a
/// raw-CAN PassThruWriteMsgs call" principle the gate already establishes for
/// a genuinely non-ISO15765-connected link
/// (`start_repeat_message_on_a_raw_can_link_never_sets_iso15765_addr_type`,
/// `repeat_message.rs`). Uses a single-frame (non-segmented) payload so
/// exactly one write is asserted.
#[tokio::test]
#[serial]
async fn software_isotp_mode_extended_addressing_never_sets_iso15765_addr_type_on_tx() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
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
            5,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                unum32_param(CP_CAN_PHYS_REQ_FORMAT, can_id_format::EXTENDED_11BIT),
                unum32_param(CP_CAN_PHYS_REQ_EXT_ADDR, 0x01),
            ],
        )],
    )
    .await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01, 0x02, 0x03],
            cop_ctrl_data: Some(vci_service_interface::ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::CAN,
        "software-isotp mode writes raw CAN frames, not ISO15765, regardless of the CLL's \
         logically-connected protocol"
    );
    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        tx_flags & j2534_0404::ISO15765_ADDR_TYPE,
        0,
        "ISO15765_ADDR_TYPE must never be set on a software-isotp mode write, since it goes out \
         under PROTOCOL_ID CAN, not ISO15765: {tx_flags:#06x}"
    );

    server.shutdown().await;
}

/// ADR-124 (superseding ADR-121): the software ISO-TP TX driver's FC-wait
/// loop tolerates consecutive `FS_WAIT` frames up to an internal defensive
/// bound (`ISOTP_TX_MAX_CONSECUTIVE_RX_WAIT_FRAMES`), not a client-configured
/// `CP_CanMaxNumWaitFrames` threshold -- there is no comparam to set up for
/// this test any more. A small number of consecutive `FS_WAIT`s followed by
/// `FS_CONTINUE_TO_SEND` proves basic WAIT tolerance under the new guard: the
/// transfer completes normally.
#[tokio::test]
#[serial]
async fn software_isotp_mode_fs_wait_then_cts_succeeds() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let payload: Vec<u8> = (1..=20).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload.clone(),
            cop_ctrl_data: Some(vci_service_interface::ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    // FirstFrame goes out immediately.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Two consecutive FS_WAIT frames, well within the internal bound. Each
    // inject is spaced out (well over the 10ms POLL_INTERVAL_MS) so each
    // frame lands in its own poll batch -- not required for correctness
    // since FcCapture queues every matching FlowControl frame in a batch,
    // but keeping this test's frames in separate batches isolates it from
    // the same-batch scenario covered by the sibling
    // `software_isotp_mode_fs_wait_then_cts_in_same_poll_batch_succeeds` test.
    let mut wait_frame = 0x7E8_u32.to_be_bytes().to_vec();
    wait_frame.extend_from_slice(&[0x31, 0x00, 0x00]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &wait_frame, j2534_0404::CAN);
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &wait_frame, j2534_0404::CAN);
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Then FlowControl: ContinueToSend, BS=0 (all), STmin=0.
    let mut fc = 0x7E8_u32.to_be_bytes().to_vec();
    fc.extend_from_slice(&[0x30, 0x00, 0x00]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &fc, j2534_0404::CAN);

    // The ConsecutiveFrames should still be written -- the transfer succeeds.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let mut expected_cf1 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf1.push(0x21);
    expected_cf1.extend_from_slice(&payload[6..13]);
    let mut expected_cf2 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf2.push(0x22);
    expected_cf2.extend_from_slice(&payload[13..20]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_cf1
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 2),
        expected_cf2
    );

    server.shutdown().await;
}

/// ADR-124 (superseding ADR-121), Decision point 2 / ADR-121 Decision point
/// 1: the WAIT counter resets per FC-wait-cycle (i.e. per block), not
/// cumulatively across a whole multi-segment transfer. A 20-byte payload
/// segments into FF + 2 ConsecutiveFrames, and with `BlockSize = 1` each
/// ConsecutiveFrame requires its own FlowControl-wait cycle. Block 1 gets
/// 600 consecutive `FS_WAIT` frames before its terminal CTS, and block 2
/// gets 600 MORE consecutive `FS_WAIT` frames before its own terminal CTS --
/// 1200 total, which exceeds the internal defensive bound
/// (`ISOTP_TX_MAX_CONSECUTIVE_RX_WAIT_FRAMES = 1027`, see `events.rs`) if
/// counted cumulatively across the transfer. The transfer succeeding proves
/// the counter genuinely resets across `while idx < chunks.len()`
/// iterations: each block's count (600) individually stays under the bound,
/// even though the sum does not. This also covers the weaker claim that
/// multi-block WAIT-handling mechanics function correctly under the
/// internal-constant guard (both ConsecutiveFrames are eventually written).
///
/// An earlier round of this test settled for only 2 WAITs per block,
/// reasoning that proving the non-cumulative-reset property at a scale that
/// actually approaches the 1027 bound would require "thousands of frames...
/// impractical". A subsequent edge-case-hunter review found that framing
/// incorrect: a 600+600 scratch repro against this exact scenario ran in
/// 0.22s, and failed (as expected) when the reset scope was broken by
/// temporarily hoisting the WAIT counter out of the per-block loop --
/// confirming both that the property is real and that testing it directly
/// is cheap.
#[tokio::test]
#[serial]
async fn software_isotp_mode_fs_wait_counter_resets_per_block_not_cumulative_across_transfer() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let payload: Vec<u8> = (1..=20).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload.clone(),
            cop_ctrl_data: Some(vci_service_interface::ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    // FirstFrame goes out immediately.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Block 1: 600 consecutive FS_WAIT frames, injected back-to-back with no
    // delay (as in `..._exceeds_internal_bound_aborts_with_prot_err`) so
    // they land across many `PassThruReadMsgs` poll batches without
    // artificial pacing.
    let mut wait_frame = 0x7E8_u32.to_be_bytes().to_vec();
    wait_frame.extend_from_slice(&[0x31, 0x00, 0x00]);
    for _ in 0..600 {
        server
            .backdoor
            .inject_rx(MOCK_CHANNEL_ID, &wait_frame, j2534_0404::CAN);
    }

    // Block 1's FlowControl: ContinueToSend, BS=1 (release one Consecutive-
    // Frame at a time), STmin=0.
    let mut fc_bs1 = 0x7E8_u32.to_be_bytes().to_vec();
    fc_bs1.extend_from_slice(&[0x30, 0x01, 0x00]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &fc_bs1, j2534_0404::CAN);

    // Exactly one ConsecutiveFrame is released -- the WAIT counter/queue for
    // this block has been consumed and reset.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let mut expected_cf1 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf1.push(0x21);
    expected_cf1.extend_from_slice(&payload[6..13]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_cf1
    );

    // Block 2: 600 MORE consecutive FS_WAIT frames. Combined with block 1's
    // 600, that's 1200 total -- over the 1027 internal bound if the counter
    // were cumulative -- but each block individually stays under it.
    for _ in 0..600 {
        server
            .backdoor
            .inject_rx(MOCK_CHANNEL_ID, &wait_frame, j2534_0404::CAN);
    }

    // Block 2's FlowControl: ContinueToSend, BS=1, STmin=0.
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &fc_bs1, j2534_0404::CAN);

    // The second ConsecutiveFrame is written -- the transfer succeeds, proving
    // the WAIT counter reset per block rather than accumulating across the
    // whole transfer.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let mut expected_cf2 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf2.push(0x22);
    expected_cf2.extend_from_slice(&payload[13..20]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 2),
        expected_cf2
    );

    server.shutdown().await;
}

/// Codex review finding (PR #130), completing ADR-121/retained under
/// ADR-124: proves the WAIT-then-terminal-CTS ordering within one poll
/// batch is handled correctly (every queued frame counted/acted on in
/// arrival order), not just that batching itself works. Two `FS_WAIT`s
/// immediately followed by a CTS all land in the SAME poll batch (no delay
/// between the three `inject_rx` calls) -- the transfer must still succeed.
/// This test's claim (same-batch WAIT-then-terminal ordering) doesn't
/// depend on the threshold value at all and remains fully valid unchanged
/// under the new internal-constant guard.
#[tokio::test]
#[serial]
async fn software_isotp_mode_fs_wait_then_cts_in_same_poll_batch_succeeds() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let payload: Vec<u8> = (1..=20).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload.clone(),
            cop_ctrl_data: Some(vci_service_interface::ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    // FirstFrame goes out immediately.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Two FS_WAIT frames immediately followed by FlowControl: ContinueToSend,
    // BS=0 (all), STmin=0. All three injected back-to-back with no delay so
    // they land in the SAME poll batch.
    let mut wait_frame = 0x7E8_u32.to_be_bytes().to_vec();
    wait_frame.extend_from_slice(&[0x31, 0x00, 0x00]);
    let mut fc = 0x7E8_u32.to_be_bytes().to_vec();
    fc.extend_from_slice(&[0x30, 0x00, 0x00]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &wait_frame, j2534_0404::CAN);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &wait_frame, j2534_0404::CAN);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &fc, j2534_0404::CAN);

    // The ConsecutiveFrames should still be written -- the transfer succeeds.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let mut expected_cf1 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf1.push(0x21);
    expected_cf1.extend_from_slice(&payload[6..13]);
    let mut expected_cf2 = 0x7E0_u32.to_be_bytes().to_vec();
    expected_cf2.push(0x22);
    expected_cf2.extend_from_slice(&payload[13..20]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_cf1
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 2),
        expected_cf2
    );

    server.shutdown().await;
}

/// ADR-124 (superseding ADR-121): exceeding the internal defensive bound
/// (`ISOTP_TX_MAX_CONSECUTIVE_RX_WAIT_FRAMES = 1027`) aborts the COP with
/// `PduErrEvtProtErr` -- the ConsecutiveFrames are never written. Replaces
/// the former `..._exceeding_wft_max_aborts_with_prot_err`,
/// `..._zero_wft_max_aborts_on_first_wait`, and
/// `..._burst_in_one_poll_batch_all_count_toward_wft_max` tests, whose
/// combined claims (overrun aborts the COP, no ConsecutiveFrames written,
/// every WAIT in a poll batch counts) are all proven by this one test
/// against the real (no longer client-configurable) bound. No comparam
/// setup is needed any more.
///
/// Injecting exactly `ISOTP_TX_MAX_CONSECUTIVE_RX_WAIT_FRAMES + 1` (1028)
/// consecutive `FS_WAIT` frames with NO delay between `inject_rx` calls is
/// deliberate: it also serves as a regression proof for the `FcCapture`
/// batch-draining fix (PR #130's earlier round). Without correct
/// batch-draining, only ~1/8th of these frames would ever be counted (one
/// per `MAX_POLL_MESSAGES`-sized poll batch), which would NOT trip this
/// bound within the test's timeout -- a batch-draining regression would
/// show up here as a hang/timeout, not just a wrong count.
#[tokio::test]
#[serial]
async fn software_isotp_mode_fs_wait_exceeds_internal_bound_aborts_with_prot_err() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let mut events = subscribe(&mut client, cll_handle).await;

    let payload: Vec<u8> = (1..=20).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload.clone(),
            cop_ctrl_data: Some(vci_service_interface::ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // 1028 consecutive FS_WAIT frames -- one over the internal bound of
    // 1027 -- injected back-to-back with no delay so they land across many
    // `PassThruReadMsgs` poll batches (MAX_POLL_MESSAGES = 8) with no
    // artificial pacing; see the doc comment above for why this also
    // regression-tests FcCapture's batch-draining.
    let mut wait_frame = 0x7E8_u32.to_be_bytes().to_vec();
    wait_frame.extend_from_slice(&[0x31, 0x00, 0x00]);
    for _ in 0..1028 {
        server
            .backdoor
            .inject_rx(MOCK_CHANNEL_ID, &wait_frame, j2534_0404::CAN);
    }

    assert!(
        wait_for_event(&mut events, 5000, |item| matches!(
            &item.data,
            Some(event_item::Data::ErrorData(error))
                if *error == vci_service_interface::PduErrorEvent::PduErrEvtProtErr as i32
        ))
        .await,
        "exceeding the internal WAIT-flood guard should fire PduErrEvtProtErr"
    );
    assert!(
        wait_for_event(&mut events, 5000, |item| matches!(
            &item.data,
            Some(event_item::Data::CopStatus(status))
                if *status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "the COP should finish after the PduErrEvtProtErr abort"
    );

    // The ConsecutiveFrames must never have been written -- only the FirstFrame.
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    drop(events);
    server.shutdown().await;
}

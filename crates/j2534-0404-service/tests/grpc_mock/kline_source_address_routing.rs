//! ADR-203: `CP_EcuRespSourceAddress`-based RX matching for KWP
//! (ISO9141/ISO14230) and SAE J1850 (VPW/PWM), through the full gRPC/poll-
//! task stack. Unit-level coverage of `route_frame`'s new protocol-gated
//! branch and `kline_j1850_source_addr` itself lives in
//! `events::bind_frame_tests` (`events_bind_frame_tests.rs`); this file
//! proves the actual client-visible behavior the design exists for:
//! per-ECU RX routing on these two protocol families, and a
//! `unique_resp_ids`-restricted `CoptSendrecv` that now actually completes
//! instead of timing out.
//!
//! Mirrors `j1939.rs`'s
//! `unique_resp_id_table_source_address_routes_matching_frame_and_drops_others`
//! for the routing/drop shape, and `response_distribution.rs`'s restricted-
//! matching tests for the `unique_resp_ids`-gated `CoptSendrecv` shape.

use serial_test::serial;
use vci_service_interface::{
    ComPrimitiveCtrlData, ExpectedResponseData, StartComPrimitiveRequest, SubscribeEventRequest,
    subscribe_event_request,
};

use crate::harness::*;

/// D-PDU `CP_EcuRespSourceAddress` ComParam ID (`PDU_PC_UNIQUE_ID` class,
/// Unum32) -- not exported by the crate, duplicated here as a literal per
/// this test suite's existing per-file `CP_*` convention (see e.g.
/// `unique_resp_id_table_binding.rs`'s own local copy of the same
/// constant).
const CP_ECU_RESP_SOURCE_ADDR: u32 = 0x8070;

/// A 4-byte KWP2000 header (format `0x80`: addressed, separate length byte)
/// with an explicit source-address byte -- the same shape
/// `concat.rs::kwp_frame_from` builds, duplicated here rather than shared
/// (this test suite's existing per-file frame-builder convention).
fn kwp_frame_from(source: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x80, 0x10, source, payload.len() as u8];
    frame.extend_from_slice(payload);
    frame
}

/// A fixed 3-byte J1850 header (format/priority, target, source) with an
/// explicit source-address byte -- the same shape
/// `rx_header_split.rs`'s J1850 tests build.
fn j1850_frame_from(source: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x68, 0x10, source];
    frame.extend_from_slice(payload);
    frame
}

/// ADR-203, mirroring `j1939.rs`'s own SA-tier routing/drop test: a KWP
/// (ISO14230) CLL with a `CP_EcuRespSourceAddress`-keyed table entry routes
/// a matching frame to that entry's own `unique_resp_identifier`, and drops
/// a frame from an unconfigured source address instead of wildcard-
/// delivering it (a table IS configured for this CLL).
#[tokio::test]
#[serial]
async fn kwp_unique_resp_id_table_source_address_routes_matching_frame_and_drops_others() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            5,
            vec![unum32_param(CP_ECU_RESP_SOURCE_ADDR, 0x21)],
        )],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    // Matching: source-address byte is 0x21.
    let payload = vec![0x62, 0xF1, 0x90];
    let matching_frame = kwp_frame_from(0x21, &payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &matching_frame, j2534_0404::ISO14230);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_eq!(result.unique_resp_identifier, 5);
    assert_result_data(&result, &matching_frame[..4], &[], &payload);

    // Non-matching: source-address byte is 0x22 -- must be dropped, not
    // wildcard-delivered.
    let non_matching_frame = kwp_frame_from(0x22, &payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &non_matching_frame, j2534_0404::ISO14230);
    assert_no_result_data(
        &mut client,
        cll_handle,
        "a KWP frame whose source address does not match the configured \
         CP_EcuRespSourceAddress entry must be dropped",
    )
    .await;

    server.shutdown().await;
}

/// Same as above, for J1850 (VPW).
#[tokio::test]
#[serial]
async fn j1850_unique_resp_id_table_source_address_routes_matching_frame_and_drops_others() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850VPW,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            5,
            vec![unum32_param(CP_ECU_RESP_SOURCE_ADDR, 0x21)],
        )],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x41, 0x00, 0x99];
    let matching_frame = j1850_frame_from(0x21, &payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &matching_frame, j2534_0404::J1850VPW);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_eq!(result.unique_resp_identifier, 5);
    assert_result_data(&result, &matching_frame[..3], &[], &payload);

    let non_matching_frame = j1850_frame_from(0x22, &payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &non_matching_frame, j2534_0404::J1850VPW);
    assert_no_result_data(
        &mut client,
        cll_handle,
        "a J1850 frame whose source address does not match the configured \
         CP_EcuRespSourceAddress entry must be dropped",
    )
    .await;

    server.shutdown().await;
}

/// ADR-203: two sibling KWP CLLs sharing one physical channel, each with a
/// distinct configured `CP_EcuRespSourceAddress`, each correctly receive
/// only frames matching their own configured address -- mirrors `j1939.rs`'s
/// own sibling-disambiguation shape (`response_distribution.rs`'s
/// `iso15765_shared_channel_routes_responses_only_to_the_matching_cll`),
/// applied to the SA tier this ADR adds.
#[tokio::test]
#[serial]
async fn kwp_siblings_are_disambiguated_by_source_address_on_a_shared_channel() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;
    // Same protocol/baud rate: both CLLs share one physical channel, so
    // every injected frame is seen by both and only routing separates them.
    assert_eq!(server.backdoor.connect_count(), 1);

    set_unique_resp_table_and_promote(
        &mut client,
        cll_a,
        vec![ecu_entry(
            1,
            vec![unum32_param(CP_ECU_RESP_SOURCE_ADDR, 0x21)],
        )],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_b,
        vec![ecu_entry(
            2,
            vec![unum32_param(CP_ECU_RESP_SOURCE_ADDR, 0x22)],
        )],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_a).await;
    arm_receive_only_monitor(&mut client, cll_b).await;

    let payload_a = vec![0x62, 0xF1, 0x90];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame_from(0x21, &payload_a),
        j2534_0404::ISO14230,
    );
    let result_a = wait_for_result_data(&mut client, cll_a).await;
    assert_eq!(result_a.unique_resp_identifier, 1);
    assert_eq!(result_a.data_bytes, payload_a);
    assert_no_result_data(
        &mut client,
        cll_b,
        "cll_b must not receive a frame addressed to cll_a's own configured ECU",
    )
    .await;

    let payload_b = vec![0x62, 0xF1, 0x91];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame_from(0x22, &payload_b),
        j2534_0404::ISO14230,
    );
    let result_b = wait_for_result_data(&mut client, cll_b).await;
    assert_eq!(result_b.unique_resp_identifier, 2);
    assert_eq!(result_b.data_bytes, payload_b);
    assert_no_result_data(
        &mut client,
        cll_a,
        "cll_a must not receive a frame addressed to cll_b's own configured ECU",
    )
    .await;

    server.shutdown().await;
}

/// ADR-203, the actual client-visible behavior this whole design exists to
/// fix: before this ADR, `route_frame` always delivered a KWP/J1850 frame
/// at the wildcard `unique_resp_identifier == 0`, so an
/// `ExpectedResponseData.unique_resp_ids`-restricted descriptor naming any
/// OTHER identifier could never match -- the COP would time out silently
/// (the P2 backlog entry this ADR closes,
/// `j2534-0404-service/docs/implementation-notes.md`). With a
/// `CP_EcuRespSourceAddress`-keyed table entry now wired into real RX
/// routing, a `unique_resp_ids`-restricted `CoptSendrecv` against a
/// matching KWP response actually COMPLETES.
#[tokio::test]
#[serial]
async fn kwp_unique_resp_ids_restricted_cop_completes_against_a_matching_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            5,
            vec![unum32_param(CP_ECU_RESP_SOURCE_ADDR, 0x21)],
        )],
    )
    .await;

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let request_payload = vec![0x22, 0xF1, 0x90];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: request_payload.clone(),
            cop_tag: None,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x62],
                    unique_resp_ids: vec![5],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let response_payload = vec![0x62, 0xF1, 0x90, 0x21];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame_from(0x21, &response_payload),
        j2534_0404::ISO14230,
    );

    let result = wait_for_cop_finished_and_result_data(&mut events, 2000).await;
    assert_eq!(result.unique_resp_identifier, 5);
    assert_eq!(result.acceptance_id, 7);
    assert_eq!(result.data_bytes, response_payload);

    // Close the event stream before shutting down: graceful shutdown waits
    // for in-flight requests, and an open SubscribeEvent stream never ends
    // (see `harness.rs`'s module doc, ADR-149).
    drop(events);

    server.shutdown().await;
}

/// Codex review finding, this PR: a KWP `START_OF_MESSAGE` indication (empty
/// `Data`, ADR-097) must still be delivered even when the CLL has a
/// `CP_EcuRespSourceAddress`-keyed table configured -- it is not an ECU
/// response and must never be routed through the new SA-matching tier.
/// Mirrors `rx_header_split.rs`'s
/// `iso14230_start_of_message_reports_empty_data_and_rx_flag`, but WITH an
/// SA table configured (that test has none, so it could not have caught
/// this regression on its own): before the `is_content_frame` gate, an
/// empty-data indication has no source byte to parse
/// (`kline_j1850_source_addr` returns `None` for `data.len() < 3`), so
/// `route_frame`'s new branch would drop it outright -- silently, before
/// `bind_frame`'s own `indication_suppressed` policy ever got a chance to
/// run.
#[tokio::test]
#[serial]
async fn kwp_start_of_message_is_still_delivered_when_a_source_address_table_is_configured() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (CP_START_MSG_IND_ENABLE, 1),
        ],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            5,
            vec![unum32_param(CP_ECU_RESP_SOURCE_ADDR, 0x21)],
        )],
    )
    .await;

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[],
        j2534_0404::ISO14230,
        0x0000_0002, // START_OF_MESSAGE
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(&result, &[], &[], &[], &[0x00, 0x00, 0x00, 0x02]);

    server.shutdown().await;
}

/// Same as above, for `RX_BREAK` -- `indication_suppressed`'s own contract
/// (`events.rs`) is that `RX_BREAK` is NEVER suppressed, so this must be
/// delivered unconditionally regardless of the SA table's contents. Mirrors
/// `rx_header_split.rs`'s `iso14230_rx_break_reports_empty_data_and_rx_flag_0x04`,
/// again with an SA table configured.
#[tokio::test]
#[serial]
async fn kwp_rx_break_is_still_delivered_when_a_source_address_table_is_configured() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            5,
            vec![unum32_param(CP_ECU_RESP_SOURCE_ADDR, 0x21)],
        )],
    )
    .await;

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[],
        j2534_0404::ISO14230,
        0x0000_0004, // RX_BREAK
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(&result, &[], &[], &[], &[0x00, 0x00, 0x00, 0x04]);

    server.shutdown().await;
}

/// Second Codex review finding, this PR: unlike the empty-payload SOM/
/// RX_BREAK cases above, a TxDone indication (`RxStatus` bits 3+0 =
/// `TX_INDICATION | TX_MSG_TYPE`, ADR-098) echoes the REAL transmitted
/// header+payload bytes -- routinely `>= 4` bytes for a realistic K-line
/// request, unlike the empty-data indications. An earlier revision of this
/// fix gated only the SA-matching call itself, leaving a non-content frame
/// with a populated (misread) `frame_can_id` to fall through to the generic
/// CAN-ID `.find()`, which can never succeed against an SA-only entry --
/// silently dropping the TxDone indication instead of delivering it,
/// violating ADR-098's "TxDone must never be filtered from delivery"
/// invariant. Must still be delivered when a `CP_EcuRespSourceAddress`
/// table is configured on this KWP CLL.
#[tokio::test]
#[serial]
async fn kwp_tx_done_is_still_delivered_when_a_source_address_table_is_configured() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400), (CP_TRANSMIT_IND_ENABLE, 1)],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            5,
            vec![unum32_param(CP_ECU_RESP_SOURCE_ADDR, 0x21)],
        )],
    )
    .await;

    // The tester's own echoed request -- a real, non-empty K-line frame
    // (4-byte header: format 0x80 addressed/separate-length-byte + target +
    // source + length byte, mirroring `rx_header_split.rs`'s own KWP header
    // tests), unlike the empty-data SOM/RX_BREAK cases above. Its own
    // source byte (0xF1, the tester's own address, NOT the configured
    // 0x21) is deliberately irrelevant here: a non-content frame must be
    // delivered regardless of whether it happens to match, mismatch, or
    // (as here) carry a byte-2 value the SA table was never configured
    // with at all.
    let echo_payload = vec![0x10, 0x01];
    let echoed_request = kwp_frame_from(0xF1, &echo_payload);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &echoed_request,
        j2534_0404::ISO14230,
        0x0000_0009, // TX_INDICATION | TX_MSG_TYPE
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(
        &result,
        &echoed_request[..4],
        &[],
        &echo_payload,
        &[0x00, 0x00, 0x00, 0x09],
    );

    server.shutdown().await;
}

//! UniqueRespIdTable response distribution (ADR-007 / ADR-014).
//!
//! Verifies `poll_rx`'s routing of received frames from the J2534 library to
//! CLLs: the CAN ID in each frame's leading 4 bytes is matched against every
//! sharing CLL's `UniqueRespIdTable`, and the frame is delivered (tagged with
//! the matching entry's `unique_resp_identifier`) only to the CLLs whose
//! table contains that CAN ID. The mock's RX injection bypasses its recorded
//! filters, so any drop observed here is the service's routing decision, not
//! adapter-level filtering.

use serial_test::serial;
use vci_service_interface::{
    ComPrimitiveCtrlData, ExpectedResponseData, StartComPrimitiveRequest, SubscribeEventRequest,
    event_item, subscribe_event_request,
};

use crate::harness::*;

/// ADR-007: two CLLs sharing one physical ISO15765 channel, each with its own
/// UniqueRespIdTable, receive only the responses whose CAN ID appears in their
/// own table — a response for CLL A's ECU never reaches CLL B and vice versa,
/// and a response matching neither table is dropped for both.
#[tokio::test]
#[serial]
async fn iso15765_shared_channel_routes_responses_only_to_the_matching_cll() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    // Same protocol and baud rate: both CLLs share one physical channel, so
    // every injected frame is seen by both and only routing separates them.
    assert_eq!(server.backdoor.connect_count(), 1);

    set_unique_resp_table_and_promote(
        &mut client,
        cll_a,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_b,
        vec![ecu_entry(
            2,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7EA),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E2),
            ],
        )],
    )
    .await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the injected
    // frames below delivered under the new unbound-discard model. cll_a and
    // cll_b share one physical channel/poll task, and a receive-only
    // registrant's wait blocks that channel's TX dispatch until its OWN
    // first match (a pre-existing S6 property -- see `arm_receive_only_
    // monitor`'s own doc comment), so cll_b's monitor cannot be armed until
    // cll_a's has already received its own first match (below) and
    // detached, freeing the channel; once detached, a registrant stays live
    // for every subsequent match too (`ReceivePhaseOutcome::
    // DetachedToTier2`), so cll_a's monitor keeps working for the rest of
    // this test without needing to be re-armed.
    arm_receive_only_monitor(&mut client, cll_a).await;

    // ECU A's response (CAN ID 0x7E8) reaches only CLL A, tagged with A's
    // entry identifier -- this is also cll_a's monitor's own first match,
    // freeing the shared channel for cll_b's monitor below.
    let payload_a = vec![0x62, 0xF1, 0x90, 0x41];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload_a),
        j2534_0404::ISO15765,
    );
    let result_a = wait_for_result_data(&mut client, cll_a).await;
    assert_result_data(&result_a, &0x7E8_u32.to_be_bytes(), &[], &payload_a);
    assert_eq!(result_a.unique_resp_identifier, 1);
    assert_no_result_data(&mut client, cll_b, "0x7E8 is not in CLL B's table").await;

    arm_receive_only_monitor(&mut client, cll_b).await;

    // ECU B's response (CAN ID 0x7EA) reaches only CLL B.
    let payload_b = vec![0x62, 0xF1, 0x91, 0x42];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7EA, &payload_b),
        j2534_0404::ISO15765,
    );
    let result_b = wait_for_result_data(&mut client, cll_b).await;
    assert_result_data(&result_b, &0x7EA_u32.to_be_bytes(), &[], &payload_b);
    assert_eq!(result_b.unique_resp_identifier, 2);
    assert_no_result_data(&mut client, cll_a, "0x7EA is not in CLL A's table").await;

    // A response from an ECU in neither table is dropped for both CLLs.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7EC, &[0x62, 0xF1, 0x92]),
        j2534_0404::ISO15765,
    );
    assert_no_result_data(&mut client, cll_a, "0x7EC is in neither table").await;
    assert_no_result_data(&mut client, cll_b, "0x7EC is in neither table").await;

    server.shutdown().await;
}

/// ADR-007: within one CLL, each response is tagged with the
/// `unique_resp_identifier` of the specific UniqueRespIdTable entry whose
/// `CP_CanRespUSDTId` matches its CAN ID — not just "some entry matched" —
/// and a CAN ID matching no entry is silently dropped even though the table
/// is non-empty.
#[tokio::test]
#[serial]
async fn iso15765_unique_resp_id_table_attributes_frames_to_matching_entry_identifiers() {
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
        vec![
            ecu_entry(
                1,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E9),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E1),
                ],
            ),
        ],
    )
    .await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps every injected
    // frame below delivered under the new unbound-discard model; this
    // test's own table entries already have CP_CanPhysReqId set.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // The second entry's ECU responds first: the frame must be attributed to
    // entry 2, proving attribution follows the CAN ID, not entry order.
    let payload_2 = vec![0x62, 0xF1, 0x91, 0x02];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E9, &payload_2),
        j2534_0404::ISO15765,
    );
    let result_2 = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result_2, &0x7E9_u32.to_be_bytes(), &[], &payload_2);
    assert_eq!(result_2.unique_resp_identifier, 2);

    let payload_1 = vec![0x62, 0xF1, 0x90, 0x01];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload_1),
        j2534_0404::ISO15765,
    );
    let result_1 = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result_1, &0x7E8_u32.to_be_bytes(), &[], &payload_1);
    assert_eq!(result_1.unique_resp_identifier, 1);

    // A CAN ID in no entry is dropped — the non-empty table is a whitelist.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7EB, &[0x62, 0xF1, 0x92]),
        j2534_0404::ISO15765,
    );
    assert_no_result_data(&mut client, cll_handle, "0x7EB matches no table entry").await;

    server.shutdown().await;
}

/// ADR-007: routing matches `CP_CanRespUUDTId` as well as `CP_CanRespUSDTId`
/// — on a (default single-channel-mode) hardware ISO15765 channel, a frame
/// from either of an entry's response CAN IDs is delivered with that entry's
/// `unique_resp_identifier`.
#[tokio::test]
#[serial]
async fn iso15765_uudt_response_can_id_routes_to_the_matching_table_entry() {
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
            4,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps every injected
    // frame below delivered under the new unbound-discard model; this
    // test's own table entry already has CP_CanPhysReqId set.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // A UUDT frame (CAN ID 0x5E8) routes to the same entry as USDT traffic.
    let uudt_payload = vec![0x62, 0xF2, 0x00, 0xAA];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x5E8, &uudt_payload),
        j2534_0404::ISO15765,
    );
    let uudt_result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&uudt_result, &0x5E8_u32.to_be_bytes(), &[], &uudt_payload);
    assert_eq!(uudt_result.unique_resp_identifier, 4);

    // The USDT response CAN ID still routes too.
    let usdt_payload = vec![0x62, 0xF1, 0x90, 0xBB];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &usdt_payload),
        j2534_0404::ISO15765,
    );
    let usdt_result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&usdt_result, &0x7E8_u32.to_be_bytes(), &[], &usdt_payload);
    assert_eq!(usdt_result.unique_resp_identifier, 4);

    // A near-miss CAN ID matching neither response ID is dropped.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x5E9, &[0x62, 0xF1, 0x92]),
        j2534_0404::ISO15765,
    );
    assert_no_result_data(
        &mut client,
        cll_handle,
        "0x5E9 matches neither response CAN ID",
    )
    .await;

    server.shutdown().await;
}

/// ADR-007's no-table mode: a CLL whose UniqueRespIdTable was never configured
/// receives every frame on its channel unconditionally with
/// `unique_resp_identifier = 0`, even while another CLL sharing the channel
/// routes by its own table — the wildcard is per-CLL, not per-channel.
///
/// Post-unbound-discard (ADR-100 Decision §5), observing this requires a live
/// receive-only monitor on each CLL (`arm_receive_only_monitor`, see that
/// helper's own doc comment) -- previously (S6) a newly-inserted receive-only
/// `-1` registrant's wait blocked its own physical channel's TX dispatch
/// until its OWN first match, so `cll_tabled` and `cll_wildcard` (sharing one
/// physical channel/poll task) could never both get their monitor armed
/// before the first frame below was injected. Fixed by giving a
/// created-receive-only `-1` registrant the same immediate-detach treatment
/// a migrated IS-CYCLIC registrant gets after its first match (ADR-100
/// Decision §2) -- it has no tier-1 phase to migrate out of, so it now
/// detaches to tier 2 and frees the poll task the instant it is inserted,
/// letting both monitors below be armed back-to-back with nothing in
/// between.
#[tokio::test]
#[serial]
async fn iso15765_cll_without_table_receives_all_frames_with_identifier_zero() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_tabled = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_wildcard = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    set_unique_resp_table_and_promote(
        &mut client,
        cll_tabled,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // cll_wildcard deliberately has no UniqueRespIdTable (that is this
    // test's whole premise), so `arm_receive_only_monitor`'s dummy,
    // never-transmitted payload cannot resolve physical (table-based) TX
    // addressing (`arm_receive_only_monitor`'s own doc comment). Functional
    // addressing resolves independently of the UniqueRespIdTable (ADR-054)
    // and does not affect RX routing, which keys purely off
    // `active_unique_resp_id_table` (empty here, preserving wildcard mode)
    // -- so this satisfies TX resolution without giving cll_wildcard a table.
    set_com_param_unum32(&mut client, cll_wildcard, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_wildcard, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    promote_via_update_param(&mut client, cll_wildcard).await;

    // Arm both CLLs' receive-only monitors before injecting anything: since
    // a created-receive-only `-1` registrant now detaches to tier 2 (and
    // frees this shared channel's poll task) the instant it is inserted,
    // neither `arm_receive_only_monitor` call below blocks behind the
    // other's own first match.
    arm_receive_only_monitor(&mut client, cll_tabled).await;
    arm_receive_only_monitor(&mut client, cll_wildcard).await;

    // A frame in the tabled CLL's table is delivered to both CLLs — with the
    // matching entry's identifier for the tabled one, identifier 0 for the
    // wildcard one.
    let payload = vec![0x62, 0xF1, 0x90, 0x41];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::ISO15765,
    );
    let tabled_result = wait_for_result_data(&mut client, cll_tabled).await;
    assert_result_data(&tabled_result, &0x7E8_u32.to_be_bytes(), &[], &payload);
    assert_eq!(tabled_result.unique_resp_identifier, 1);
    let wildcard_result = wait_for_result_data(&mut client, cll_wildcard).await;
    assert_result_data(&wildcard_result, &0x7E8_u32.to_be_bytes(), &[], &payload);
    assert_eq!(wildcard_result.unique_resp_identifier, 0);

    // A frame in nobody's table still reaches the wildcard CLL (identifier 0)
    // but is dropped for the tabled CLL.
    let stray_payload = vec![0x01, 0x02, 0x03];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x123, &stray_payload),
        j2534_0404::ISO15765,
    );
    let stray_result = wait_for_result_data(&mut client, cll_wildcard).await;
    assert_result_data(&stray_result, &0x123_u32.to_be_bytes(), &[], &stray_payload);
    assert_eq!(stray_result.unique_resp_identifier, 0);
    assert_no_result_data(
        &mut client,
        cll_tabled,
        "0x123 is not in the tabled CLL's table",
    )
    .await;

    server.shutdown().await;
}

/// ADR-014: `ExpectedResponseData.unique_resp_ids` gates which
/// UniqueRespIdTable entries' ECUs may finish a `CoptSendrecv` — a response
/// from an excluded ECU is still *delivered* (routing and frame delivery are
/// unaffected, ADR-014 §3) but does not complete the COP even though it
/// matches the mask/pattern; the selected ECU's response then finishes it with
/// the descriptor's `acceptance_id`.
#[tokio::test]
#[serial]
async fn iso15765_sendrecv_unique_resp_ids_filter_gates_expected_response_matching() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // CP_P2Max (µs) is the CoptSendrecv response window (ADR-053); widen it
    // to 5 s so the deliberate ~300 ms wrong-ECU phase below cannot expire it.
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 5_000_000),
        ],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![
            ecu_entry(
                1,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E9),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E1),
                ],
            ),
        ],
    )
    .await;

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the excluded
    // ECU's response below delivered under the new unbound-discard model
    // (it is genuinely unbound: excluded by the pending COP's own
    // unique_resp_ids gate at tier-1, and nothing else is listening) --
    // this is the ADR-014 "delivery is unaffected by the gate" claim below,
    // now dependent on this migration path. Armed here, before the pending
    // COP starts (which would otherwise occupy this shared channel's poll
    // task in its own blocking receive phase until entry 2 responds, per a
    // pre-existing S6 property -- see `arm_receive_only_monitor`'s own doc
    // comment), and unblocked immediately by a throwaway priming frame so
    // the pending COP's own dispatch below is never stuck behind it; once
    // detached (`ReceivePhaseOutcome::DetachedToTier2`) the monitor stays
    // live for the excluded-ECU frame later in this test without needing to
    // be re-armed.
    arm_receive_only_monitor(&mut client, cll_handle).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x00]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "the priming frame should prove the receive-only monitor is live"
    );

    // Wait only for entry 2's ECU: mask/pattern accepts any positive
    // ReadDataByIdentifier response (first payload byte 0x62), so without the
    // unique_resp_ids gate entry 1's ECU could satisfy it.
    let request_payload = vec![0x22, 0xF1, 0x90];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: request_payload.clone(),
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                // Time is the cyclic-send cycle time (ADR-053), not the
                // response window — that is CP_P2Max above.
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x62],
                    unique_resp_ids: vec![2],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    // The service starts waiting for a matching RX frame only after the
    // request's PassThruWriteMsgs completes; inject nothing before then.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    // ADR-050: the TX header comes from the table's *first* entry.
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        can_frame(0x7E0, &request_payload)
    );

    // Entry 1's ECU responds first. The frame matches the mask/pattern but
    // its identifier (1) is not in unique_resp_ids = [2].
    let wrong_ecu_payload = vec![0x62, 0xF1, 0x90, 0x11];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &wrong_ecu_payload),
        j2534_0404::ISO15765,
    );

    // Delivery is unaffected by the gate: the frame reaches the CLL tagged
    // with entry 1's identifier, but unaccepted (acceptance_id 0)...
    let mut wrong_ecu: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                wrong_ecu = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "the excluded ECU's response should still be delivered"
    );
    let wrong_ecu = wrong_ecu.expect("ResultData should have been captured");
    assert_eq!(wrong_ecu.unique_resp_identifier, 1);
    assert_eq!(wrong_ecu.acceptance_id, 0);
    assert_eq!(wrong_ecu.data_bytes, wrong_ecu_payload);

    // ...and the COP must not finish on it.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv must not finish on an ECU excluded by unique_resp_ids"
    );

    // Entry 2's ECU responds: delivered with its identifier and the
    // descriptor's acceptance_id, and the COP finishes.
    let selected_ecu_payload = vec![0x62, 0xF1, 0x90, 0x22];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E9, &selected_ecu_payload),
        j2534_0404::ISO15765,
    );

    let mut selected_ecu: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                selected_ecu = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "the selected ECU's response should be delivered"
    );
    let selected_ecu = selected_ecu.expect("ResultData should have been captured");
    assert_eq!(selected_ecu.unique_resp_identifier, 2);
    assert_eq!(selected_ecu.acceptance_id, 7);
    assert_eq!(selected_ecu.data_bytes, selected_ecu_payload);

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv should finish on the selected ECU's response"
    );

    // Close the event stream before shutting down: graceful shutdown waits
    // for in-flight requests, and an open SubscribeEvent stream never ends.
    drop(events);

    server.shutdown().await;
}

/// ADR-097: a `START_OF_MESSAGE` frame (`RxStatus` bit 1 set) arriving during
/// a pending `CoptSendrecv` wait is delivered as an ordinary unsolicited
/// indication, but must never be attributed as the awaited response --
/// `ExpectedResponse::matches` is vacuously `true` on an empty payload
/// whenever `mask`/`pattern` is empty, and a SOM frame's payload is always
/// empty after the ADR-051 header split, so without the `poll_rx_inner`
/// exclusion this would falsely complete the COP before the ECU's real
/// response arrives.
#[tokio::test]
#[serial]
async fn iso15765_start_of_message_frame_does_not_complete_a_pending_sendrecv() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Widen CP_P2Max so the deliberate SOM-then-real-response sequence below
    // cannot expire the response window.
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        // ADR-151: CP_StartMsgIndEnable defaults to disabled -- explicitly
        // enabled here so the SOM indication this test exercises actually
        // reaches the client.
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 5_000_000),
            (CP_START_MSG_IND_ENABLE, 1),
        ],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
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

    // Empty mask/pattern: matches anything, including a vacuous empty
    // payload -- deliberately broad, to prove the SOM exclusion is what
    // stops the false match, not the descriptor happening to be narrow.
    let request_payload = vec![0x22, 0xF1, 0x90];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: request_payload.clone(),
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A START_OF_MESSAGE indication arrives first: header-only Data (just
    // the CAN ID), no payload.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::ISO15765,
        0x0000_0002, // START_OF_MESSAGE
    );

    // It is still delivered -- as an unsolicited indication, unattributed to
    // the pending COP (no cop_handle on the EventItem itself).
    let mut som_result: Option<vci_service_interface::ResultData> = None;
    let mut som_cop_handle: Option<vci_service_interface::ComPrimitiveHandle> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                som_result = Some(result.clone());
                som_cop_handle = item.cop_handle;
                return true;
            }
            false
        })
        .await,
        "the START_OF_MESSAGE frame should still be delivered"
    );
    let som_result = som_result.expect("ResultData should have been captured");
    assert_eq!(som_result.data_bytes, Vec::<u8>::new());
    assert_eq!(som_result.rx_flag, vec![0x00, 0x00, 0x00, 0x02]);
    assert_eq!(som_result.acceptance_id, 0);
    assert_eq!(
        som_cop_handle, None,
        "a START_OF_MESSAGE frame must not be attributed to the pending COP"
    );
    // ADR-143: START_OF_MESSAGE alone populates start_msg_timestamp and
    // leaves tx_msg_done_timestamp unset -- the two fields key off
    // independent RxStatus bits.
    assert_eq!(som_result.start_msg_timestamp, Some(MOCK_TIMESTAMP));
    assert_eq!(som_result.tx_msg_done_timestamp, None);

    // ...and must not finish the COP, even though the (empty mask/pattern)
    // descriptor would vacuously match its empty payload.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv must not finish on a START_OF_MESSAGE frame"
    );

    // The ECU's real (reassembled, non-SOM) response now arrives and
    // completes the COP normally.
    let response_payload = vec![0x62, 0xF1, 0x90];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &response_payload),
        j2534_0404::ISO15765,
    );

    let mut real_result: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                real_result = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "the real response should be delivered"
    );
    let real_result = real_result.expect("ResultData should have been captured");
    assert_eq!(real_result.data_bytes, response_payload);
    assert_eq!(real_result.rx_flag, Vec::<u8>::new());
    assert_eq!(real_result.acceptance_id, 7);

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv should finish on the real response"
    );

    drop(events);

    server.shutdown().await;
}

/// ADR-098: a TxDone indication (`RxStatus` bits 3+0 = `TX_INDICATION` |
/// `TX_MSG_TYPE`, `0x09`) arriving during a pending `CoptSendrecv` wait must
/// never be attributed as the awaited response, for the same vacuous-match
/// reason as a `START_OF_MESSAGE` frame (see
/// `iso15765_start_of_message_frame_does_not_complete_a_pending_sendrecv`):
/// a broad/empty expected-response descriptor would otherwise vacuously
/// match its post-split payload. Before ADR-098, only `START_OF_MESSAGE`
/// frames were excluded -- a TxDone indication (no SOM bit) was not, so it
/// could falsely complete a pending native-ISO15765 `CoptSendrecv` wait.
#[tokio::test]
#[serial]
async fn iso15765_tx_done_frame_does_not_complete_a_pending_sendrecv() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        // ADR-151: CP_TransmitIndEnable defaults to disabled -- explicitly
        // enabled here so the TxDone indication this test exercises actually
        // reaches the client.
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 5_000_000),
            (CP_TRANSMIT_IND_ENABLE, 1),
        ],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
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

    // Empty mask/pattern: matches anything, including a vacuous empty
    // payload -- deliberately broad, same rationale as the SOM test above.
    let request_payload = vec![0x22, 0xF1, 0x90];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: request_payload.clone(),
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A TxDone indication arrives: header-only Data, RxStatus 0x09.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::ISO15765,
        0x0000_0009, // TX_INDICATION | TX_MSG_TYPE
    );

    // It is still delivered -- as an unsolicited indication, unattributed to
    // the pending COP.
    let mut tx_done_result: Option<vci_service_interface::ResultData> = None;
    let mut tx_done_cop_handle: Option<vci_service_interface::ComPrimitiveHandle> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                tx_done_result = Some(result.clone());
                tx_done_cop_handle = item.cop_handle;
                return true;
            }
            false
        })
        .await,
        "the TxDone frame should still be delivered"
    );
    let tx_done_result = tx_done_result.expect("ResultData should have been captured");
    assert_eq!(tx_done_result.data_bytes, Vec::<u8>::new());
    assert_eq!(tx_done_result.rx_flag, vec![0x00, 0x00, 0x00, 0x09]);
    assert_eq!(tx_done_result.acceptance_id, 0);
    assert_eq!(
        tx_done_cop_handle, None,
        "a TxDone frame must not be attributed to the pending COP"
    );
    // ADR-143: TX_INDICATION alone populates tx_msg_done_timestamp and
    // leaves start_msg_timestamp unset, even though TX_MSG_TYPE (loopback)
    // is also set in this 0x09 frame -- each field keys off its own bit
    // only.
    assert_eq!(tx_done_result.tx_msg_done_timestamp, Some(MOCK_TIMESTAMP));
    assert_eq!(tx_done_result.start_msg_timestamp, None);

    // ...and must not finish the COP, even though the (empty mask/pattern)
    // descriptor would vacuously match its empty payload.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv must not finish on a TxDone frame"
    );

    // The ECU's real response now arrives and completes the COP normally.
    let response_payload = vec![0x62, 0xF1, 0x90];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &response_payload),
        j2534_0404::ISO15765,
    );

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv should finish on the real response"
    );

    drop(events);

    server.shutdown().await;
}

/// ADR-098: a CONFIG_LOOPBACK echo of our own transmitted request (`RxStatus`
/// bit 0 = `TX_MSG_TYPE`, `0x01`) must not register a phantom pending-RC.
/// `RcHandlingConfig::detect_pending_rc` reads `payload[CP_RCByteOffset]` and
/// treats a matching pending-NRC byte value as reason to extend the wait and
/// attribute `cop_handle` to that frame -- with `CP_RC23Handling` enabled,
/// an echoed SID 0x23 (ReadMemoryByAddress) request's own first byte is
/// byte-identical to NRC 0x23 (RequestSequenceError/ConditionsNotCorrect),
/// so the pending-RC check (which runs before the mask/pattern match check)
/// would otherwise mistake our own echoed request for a genuine pending NRC.
/// Before ADR-098, only `START_OF_MESSAGE` frames were excluded from this
/// detection -- a plain loopback echo (no SOM bit) was not.
#[tokio::test]
#[serial]
async fn iso15765_loopback_echo_of_own_request_does_not_register_phantom_pending_rc() {
    const CP_RC23_HANDLING: u32 = 0x8024;
    const CP_RC_BYTE_OFFSET: u32 = 0x8028;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 5_000_000),
            (CP_RC23_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 0),
        ],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
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

    // Empty mask/pattern: broad enough to vacuously match too, isolating
    // that the pending-RC exclusion (not the match exclusion) is what stops
    // this specific frame from being attributed.
    let request_payload = vec![0x23, 0x04, 0x00, 0x00, 0x10, 0x00, 0x04];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: request_payload.clone(),
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A CONFIG_LOOPBACK echo of our own request, on the ECU's USDT response
    // CAN ID so it routes to this CLL, carrying the same SID 0x23 first
    // byte -- must not be treated as a pending NRC 0x23.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &request_payload),
        j2534_0404::ISO15765,
        0x0000_0001, // TX_MSG_TYPE (loopback echo)
    );

    let mut loopback_result: Option<vci_service_interface::ResultData> = None;
    let mut loopback_cop_handle: Option<vci_service_interface::ComPrimitiveHandle> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                loopback_result = Some(result.clone());
                loopback_cop_handle = item.cop_handle;
                return true;
            }
            false
        })
        .await,
        "the loopback frame should still be delivered"
    );
    let loopback_result = loopback_result.expect("ResultData should have been captured");
    assert_eq!(loopback_result.rx_flag, vec![0x00, 0x00, 0x00, 0x01]);
    assert_eq!(
        loopback_cop_handle, None,
        "a loopback echo must not be attributed to the pending COP as a pending-RC frame \
         (this would have been Some(..) before ADR-098, via the pending-RC branch which runs \
         before the mask/pattern match check)"
    );
    // ADR-143: a bare TX_MSG_TYPE (loopback) echo, with neither
    // TX_INDICATION nor START_OF_MESSAGE set, populates neither timestamp
    // field. This is a deliberate abstention, not an oversight -- keying
    // tx_msg_done_timestamp off TX_MSG_TYPE alone would mislabel the
    // ADR-097 TX_MSG_TYPE|START_OF_MESSAGE (0x03) combo's start-of-message
    // time as a done-time, and disambiguating the two would need the same
    // multi-bit combination logic ADR-098 already declined to add. The
    // client can already derive this information from EventItem.timestamp
    // plus the existing TX_MSG_TYPE rx_flag bit.
    assert_eq!(loopback_result.tx_msg_done_timestamp, None);
    assert_eq!(loopback_result.start_msg_timestamp, None);

    // ...and must not finish the COP.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv must not finish on the loopback echo"
    );

    // The ECU's real response now arrives and completes the COP normally.
    let response_payload = vec![
        0x63, 0x04, 0x00, 0x00, 0x10, 0x00, 0x04, 0x01, 0x02, 0x03, 0x04,
    ];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &response_payload),
        j2534_0404::ISO15765,
    );

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv should finish on the real response"
    );

    drop(events);

    server.shutdown().await;
}

/// ADR-098 (corrected): unlike `START_OF_MESSAGE`/`TX_INDICATION`/`RX_BREAK`/
/// `TX_MSG_TYPE`, `ISO15765_PADDING_ERROR` (`RxStatus` bit 4, `0x10`) tags a
/// genuine, fully reassembled ISO15765 response whose final CAN frame simply
/// had fewer than 8 data bytes -- not an indication. A frame tagged with only
/// this bit set must remain eligible for `ExpectedResponse` matching and
/// complete a pending `CoptSendrecv` normally, with `cop_handle` attributed.
/// Before the correction, the blanket `rx_status_flags == 0` guard excluded
/// this frame too, so a legitimate matching response would never complete the
/// wait and the COP would instead time out at `CP_P2Max`.
#[tokio::test]
#[serial]
async fn iso15765_padding_error_frame_completes_a_pending_sendrecv() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 5_000_000),
        ],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
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
    let cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: request_payload.clone(),
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
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // The ECU's response arrives on a CAN frame with fewer than 8 data bytes
    // (4-byte CAN-ID header + 3-byte payload = 7 total), tagged with
    // ISO15765_PADDING_ERROR only.
    let response_payload = vec![0x62, 0xF1, 0x90];
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &response_payload),
        j2534_0404::ISO15765,
        0x0000_0010, // ISO15765_PADDING_ERROR
    );

    let mut result: Option<vci_service_interface::ResultData> = None;
    let mut result_cop_handle: Option<vci_service_interface::ComPrimitiveHandle> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result_data)) = &item.data {
                result = Some(result_data.clone());
                result_cop_handle = item.cop_handle;
                return true;
            }
            false
        })
        .await,
        "the padding-error-tagged response should be delivered"
    );
    let result = result.expect("ResultData should have been captured");
    assert_eq!(result.data_bytes, response_payload);
    assert_eq!(result.rx_flag, vec![0x00, 0x00, 0x00, 0x10]);
    assert_eq!(result.acceptance_id, 7);
    assert_eq!(
        result_cop_handle,
        Some(cop_handle),
        "a padding-error-tagged matching response must be attributed to the pending COP"
    );

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv should finish on the padding-error-tagged matching response"
    );

    drop(events);

    server.shutdown().await;
}

/// ADR-098 (corrected): a `7F SS 78` (response-pending NRC) frame is exactly
/// 3 bytes, so an ECU that does not pad its CAN frames always sends it on a
/// CAN frame with fewer than 8 data bytes -- `ISO15765_PADDING_ERROR` is
/// therefore set on essentially every such frame. The corrected guard must
/// still route this frame through `detect_pending_rc`, extending the pending
/// `CoptSendrecv` wait, exactly as an unpadded (`RxStatus == 0`) `0x78` frame
/// would. Before the correction, the blanket `rx_status_flags == 0` guard
/// meant `detect_pending_rc` was never reached for such an ECU, so
/// `CP_RC78Handling` was systematically broken for it.
#[tokio::test]
#[serial]
async fn iso15765_padding_error_response_pending_frame_extends_pending_rc_wait() {
    const CP_RC78_HANDLING: u32 = 0x8027;
    const CP_RC_BYTE_OFFSET: u32 = 0x8028;
    const CP_P2_STAR: u32 = 0x8011;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            // Deliberately much larger than the RC78 ceiling below, so the
            // base response window can never be what ends the COP.
            (j2534_0404::P2_MAX, 10_000_000),
            (CP_RC78_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_P2_STAR, 300_000), // 300 ms ceiling, stored in us (ADR-057)
        ],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
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

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x22, 0xF1, 0x90],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 1,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x62],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // 0x78 at t=0, tagged with ISO15765_PADDING_ERROR (3-byte payload on an
    // unpadded ECU's CAN frame): establishes the ~300 ms ceiling via
    // detect_pending_rc, same as an untagged 0x78 would.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x22, 0x78]),
        j2534_0404::ISO15765,
        0x0000_0010, // ISO15765_PADDING_ERROR
    );

    // No final positive response ever arrives, so the COP can only end via
    // the RC78 ceiling -- proving detect_pending_rc was actually reached and
    // registered the pending-RC wait for this padding-error-tagged frame.
    assert!(
        wait_for_event(&mut events, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "COP should finish at the RC78 ceiling, proving the padding-error-tagged \
         0x78 frame reached detect_pending_rc and registered the pending-RC wait"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-098 (corrected): `ISO15765_PADDING_ERROR` combined with any of the 4
/// indication-type bits (here `TX_MSG_TYPE`/loopback, `PAD|LOOPBACK = 0x11`)
/// must remain excluded from `ExpectedResponse`/pending-RC matching -- the
/// carve-out is specific to padding-error being the *only* bit set, not a
/// blanket allowance whenever padding-error is present.
#[tokio::test]
#[serial]
async fn iso15765_padding_error_combined_with_loopback_still_excluded() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 5_000_000),
        ],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
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

    // Empty mask/pattern: matches anything, including a vacuous empty
    // payload -- deliberately broad, same rationale as the SOM/TxDone tests.
    let request_payload = vec![0x22, 0xF1, 0x90];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: request_payload.clone(),
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A loopback echo of our own request, additionally (implausibly, but the
    // guard must still hold) tagged with ISO15765_PADDING_ERROR: RxStatus ==
    // 0x11 (PAD | TX_MSG_TYPE).
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::ISO15765,
        0x0000_0011, // ISO15765_PADDING_ERROR | TX_MSG_TYPE
    );

    let mut result: Option<vci_service_interface::ResultData> = None;
    let mut result_cop_handle: Option<vci_service_interface::ComPrimitiveHandle> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result_data)) = &item.data {
                result = Some(result_data.clone());
                result_cop_handle = item.cop_handle;
                return true;
            }
            false
        })
        .await,
        "the PAD|LOOPBACK frame should still be delivered"
    );
    let result = result.expect("ResultData should have been captured");
    assert_eq!(result.rx_flag, vec![0x00, 0x00, 0x00, 0x11]);
    assert_eq!(
        result_cop_handle, None,
        "a PAD|LOOPBACK frame must not be attributed to the pending COP"
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv must not finish on a PAD|LOOPBACK frame"
    );

    // The ECU's real response now arrives and completes the COP normally.
    let response_payload = vec![0x62, 0xF1, 0x90];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &response_payload),
        j2534_0404::ISO15765,
    );

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv should finish on the real response"
    );

    drop(events);

    server.shutdown().await;
}

/// ADR-098 Correction: the padding-error carve-out (`rx_status_flags &
/// !RX_ISO15765_PADDING_ERROR == 0`) only clears bit 4 -- any frame that ALSO
/// carries SOM, RxBreak, or TxDone alongside padding-error must remain
/// excluded from `ExpectedResponse`/pending-RC matching, the same as the
/// PAD|LOOPBACK case above. Covers the three combinations that test did not.
#[tokio::test]
#[serial]
async fn iso15765_padding_error_combined_with_other_indication_bits_still_excluded() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        // ADR-151: CP_StartMsgIndEnable/CP_TransmitIndEnable default to
        // disabled -- both explicitly enabled here so the PAD|SOM and
        // PAD|TX_INDICATION combos this test exercises actually reach the
        // client (PAD|RX_BREAK is unaffected, per ADR-151, either way).
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 5_000_000),
            (CP_START_MSG_IND_ENABLE, 1),
            (CP_TRANSMIT_IND_ENABLE, 1),
        ],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
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

    // Empty mask/pattern: matches anything, including a vacuous empty
    // payload -- deliberately broad, same rationale as the sibling test.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x22, 0xF1, 0x90],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // PAD|SOM (0x12), PAD|RX_BREAK (0x14), PAD|TX_INDICATION (0x18): each
    // must still be excluded -- the carve-out is bit-4-only, not "padding
    // error present anywhere in the byte".
    for rx_status in [0x0000_0012_u32, 0x0000_0014, 0x0000_0018] {
        server.backdoor.inject_rx_with_status(
            MOCK_CHANNEL_ID,
            &can_frame(0x7E8, &[]),
            j2534_0404::ISO15765,
            rx_status,
        );

        let mut result: Option<vci_service_interface::ResultData> = None;
        let mut result_cop_handle: Option<vci_service_interface::ComPrimitiveHandle> = None;
        assert!(
            wait_for_event(&mut events, 2000, |item| {
                if let Some(event_item::Data::ResultData(result_data)) = &item.data {
                    result = Some(result_data.clone());
                    result_cop_handle = item.cop_handle;
                    return true;
                }
                false
            })
            .await,
            "the 0x{rx_status:04x} frame should still be delivered"
        );
        let result = result.expect("ResultData should have been captured");
        assert_eq!(result.rx_flag, vec![0x00, 0x00, 0x00, rx_status as u8]);
        assert_eq!(
            result_cop_handle, None,
            "a 0x{rx_status:04x} frame must not be attributed to the pending COP"
        );
    }

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv must not finish on any padding-error-combined frame"
    );

    // The ECU's real response now arrives and completes the COP normally.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90]),
        j2534_0404::ISO15765,
    );

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv should finish on the real response"
    );

    drop(events);

    server.shutdown().await;
}

/// ADR-143: ADR-097's documented `TX_MSG_TYPE | START_OF_MESSAGE` combo
/// (`RxStatus` `0x03`) is the decisive counter-example for why
/// `tx_msg_done_timestamp` is not keyed off `TX_MSG_TYPE` -- this frame's
/// timestamp is a start-of-first-bit time, not a done time, so only
/// `start_msg_timestamp` (keyed off `START_OF_MESSAGE` alone) should be
/// populated even though `TX_MSG_TYPE` (loopback) is also set.
#[tokio::test]
#[serial]
async fn iso15765_loopback_and_som_combo_populates_start_msg_timestamp_only() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        // ADR-151: CP_StartMsgIndEnable is deliberately left at its default
        // (disabled) here -- a bare TX_MSG_TYPE bit (loopback) is an
        // independent delivery justification `indication_suppressed` never
        // gates on this ComParam, so the TX_MSG_TYPE|SOM combo below must
        // still reach the client even with the ComParam unset. This also
        // closes an end-to-end coverage gap Codex review flagged on PR #23:
        // without this, only a unit test proved the combination survives at
        // the real spec default.
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::ISO15765,
        0x0000_0003, // TX_MSG_TYPE | START_OF_MESSAGE
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_eq!(result.rx_flag, vec![0x00, 0x00, 0x00, 0x03]);
    assert_eq!(result.start_msg_timestamp, Some(MOCK_TIMESTAMP));
    assert_eq!(result.tx_msg_done_timestamp, None);

    server.shutdown().await;
}

/// ADR-143: `tx_msg_done_timestamp` keys off `TX_INDICATION` alone, no
/// combination validation, matching ADR-098's existing trust-the-hardware
/// idiom -- a frame additionally tagged with `TX_MSG_TYPE` (loopback) and
/// `ISO15765_PADDING_ERROR` (`RxStatus` `0x19`) still populates it.
#[tokio::test]
#[serial]
async fn iso15765_tx_indication_combined_with_padding_error_populates_tx_msg_done_timestamp() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        // ADR-151: CP_TransmitIndEnable defaults to disabled -- explicitly
        // enabled here so the TX_INDICATION|TX_MSG_TYPE|PAD combo this test
        // exercises actually reaches the client.
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_TRANSMIT_IND_ENABLE, 1),
        ],
    )
    .await;

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::ISO15765,
        0x0000_0019, // TX_INDICATION | TX_MSG_TYPE | ISO15765_PADDING_ERROR
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_eq!(result.rx_flag, vec![0x00, 0x00, 0x00, 0x19]);
    assert_eq!(result.tx_msg_done_timestamp, Some(MOCK_TIMESTAMP));
    assert_eq!(result.start_msg_timestamp, None);

    server.shutdown().await;
}

/// ADR-100 Decision §5 (S8): a response that matches no ComPrimitive's
/// ExpectedResponseStructure is discarded.
/// A content frame that routes past the UniqueRespIdTable (so this is
/// `bind_frame`'s unbound-discard, not the ADR-007 table-routing drop
/// already covered above) but matches no registrant and no armed
/// tester-present window is now dropped outright: not buffered, and not
/// retrievable via `GetEventItem` even after giving the poll task time to
/// fan it out.
#[tokio::test]
#[serial]
async fn iso15765_unbound_content_frame_is_discarded() {
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
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // No COP ever started on this CLL, and tester-present was never armed
    // (CoptStartcomm never called) -- nothing is a binding candidate, so
    // every one of ADR-100 Decision §3's steps 2-5 falls through to step 6.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );

    assert_no_result_data(
        &mut client,
        cll_handle,
        "an unbound content frame must be discarded per ADR-100 Decision §5, not delivered as \
         unsolicited ResultData",
    )
    .await;

    server.shutdown().await;
}

/// ADR-100 Decision §5's own migration path: a `NumSendCycles == 0`
/// (ADR-059) receive-only ComPrimitive with a broad/vacuous
/// `ExpectedResponseStructure` still receives a frame that would otherwise
/// now be dropped by S8's discard flip -- this is the spec-conformant
/// bus-monitoring replacement for the old blanket unsolicited-delivery
/// behavior, and already worked before this step (S6 only added
/// `CP_CyclicRespTimeout`, not this basic ADR-059 creation semantics).
#[tokio::test]
#[serial]
async fn iso15765_receive_only_vacuous_com_primitive_still_receives_an_otherwise_unbound_frame() {
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
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // NumSendCycles = 0 (receive-only, ADR-059), NumReceiveCycles = -1
    // (IS-CYCLIC receive, so this registers directly as tier-2 per ADR-100
    // Decision §4/S6), broad/vacuous ExpectedResponseStructure (empty
    // mask/pattern -- the spec's own "broad monitoring" sanction for this
    // tier, ADR-100 Decision §3 step 5).
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 42,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv, receive-only) should succeed");

    // Give the poll task time to register the receive-only COP before
    // injecting; NumSendCycles = 0 must never transmit.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "NumSendCycles = 0 must not transmit anything"
    );

    let payload = vec![0x62, 0xF1, 0x90, 0x01];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::ISO15765,
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x7E8_u32.to_be_bytes(), &[], &payload);
    assert_eq!(
        result.acceptance_id, 42,
        "the receive-only monitoring COP should still receive the frame S8 would otherwise \
         discard"
    );

    server.shutdown().await;
}

/// Contrast case for the two tests above: a VACUOUS tier-1 registrant (an
/// ordinary `CoptSendrecv`, `NumSendCycles = 1`, with an empty-mask/pattern
/// descriptor) still wins at ADR-100 Decision §3 step 4 for a frame nothing
/// else claims -- this is NOT the S8 discard case. S8 only fires when
/// `bind_frame` falls all the way through to step 6 (genuinely unbound by
/// EVERY registrant, tier-1 and tier-2, and by tester-present); a vacuous
/// tier-1 claim at step 4 short-circuits before step 6 is ever reached, so
/// this frame is delivered exactly as it was before ADR-100.
#[tokio::test]
#[serial]
async fn iso15765_vacuous_tier1_registrant_still_receives_an_otherwise_unbound_frame() {
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
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // An ordinary send-and-receive COP (tier-1, NOT receive-only), with a
    // vacuous (empty mask/pattern) ExpectedResponseStructure, and no
    // tester-present armed to outrank it.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x22, 0xF1, 0x90],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 13,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let payload = vec![0x62, 0xF1, 0x90, 0x01];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::ISO15765,
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x7E8_u32.to_be_bytes(), &[], &payload);
    assert_eq!(
        result.acceptance_id, 13,
        "a vacuous tier-1 registrant must still win an otherwise-unbound frame -- distinct from \
         the genuinely-unbound, now-discarded case"
    );

    server.shutdown().await;
}

/// Direct regression test for this ADR's own gap fix (ADR-100 Decision §2 /
/// §4, S6/S8): a created-receive-only (`NumSendCycles == 0`, ADR-059)
/// `NumReceiveCycles == -1` registrant is tier-2 (`RegistrantTier::
/// ReceiveOnly`) from the instant it is inserted -- it has no active-send-
/// and-receive phase to migrate out of, unlike a migrated IS-CYCLIC
/// registrant (S5), so it must free its physical channel's poll task the
/// instant it is inserted rather than blocking inline for its own first
/// match. Before this fix, `wait_for_expected_response_inner`'s `no_deadline`
/// loop waited inline for exactly that first match regardless, wedging the
/// channel's TX dispatch for every other queued item -- the same wedge S5
/// already closed for IS-CYCLIC, left open here (see
/// `j2534-0404-service/docs/implementation-notes.md` for the full gap
/// analysis this test used to be blocked by; `iso15765_cll_without_table_
/// receives_all_frames_with_identifier_zero` and `stopcomm_data_tx.rs`'s
/// `stopcomm_receive_phase_reconnect_mid_wait_does_not_misattribute_new_
/// frame_to_stale_cop` were left `#[ignore]`d for exactly this reason until
/// this fix).
///
/// 1. Arms a broad (vacuous `ExpectedResponseStructure`) receive-only monitor
///    on `cll_a`, with NO matching frame ever injected for it during the
///    whole test -- before the fix, this alone would permanently occupy the
///    shared physical channel's poll task, since nothing would ever free it.
/// 2. A sibling `CoptSendrecv` on the SAME CLL (`cll_a`) still dispatches its
///    write and reaches `PduCopstFinished` within a bounded window -- proving
///    the still-unmatched monitor never wedges a same-CLL sibling COP, not
///    just a cross-CLL one.
/// 3. A second broad receive-only monitor is armed on `cll_b` (sharing the
///    same physical channel) immediately afterward, with nothing
///    synchronizing the two arm calls -- before the fix, this could never
///    even be inserted, since `cll_a`'s own monitor (never matched) would
///    permanently occupy TX dispatch.
/// 4. A single frame, routed to both CLLs by their own tables, is then
///    delivered to BOTH monitors independently -- proving both stayed live
///    and functional simultaneously (not merely successfully armed and then
///    silently starved).
#[tokio::test]
#[serial]
async fn receive_only_monitors_do_not_wedge_sibling_cops_or_each_other() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );

    set_unique_resp_table_and_promote(
        &mut client,
        cll_a,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_b,
        vec![ecu_entry(
            2,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E2),
            ],
        )],
    )
    .await;

    let mut events_a = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Step 1: arm cll_a's monitor. No frame is ever injected that it could
    // match during the whole test -- it sits live, permanently unmatched.
    arm_receive_only_monitor(&mut client, cll_a).await;

    // Step 2: a sibling CoptSendrecv on the SAME CLL still dispatches and
    // finishes within a bounded window. Its own descriptor is non-vacuous
    // (tier-1), so per ADR-100 Decision §3's precedence table it -- not
    // cll_a's monitor -- claims the response injected below; cll_a's monitor
    // stays unmatched throughout this step, exactly as step 1 intends.
    let request_payload = vec![0x22, 0xF1, 0x90];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: request_payload.clone(),
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
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        can_frame(0x7E0, &request_payload),
        "the sibling COP's own request should have been written -- cll_a's still-unmatched \
         receive-only monitor must not wedge this CLL's own TX dispatch"
    );

    let sibling_payload = vec![0x62, 0xF1, 0x90, 0x55];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &sibling_payload),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "the sibling CoptSendrecv should finish within a bounded window -- cll_a's still- \
         unmatched receive-only monitor must not wedge it"
    );

    // Step 3: arm cll_b's monitor immediately, with nothing synchronizing
    // the two arm calls -- before the fix this TxItem could never even be
    // dispatched, since cll_a's own (still unmatched) monitor would
    // permanently occupy TX dispatch on the shared channel.
    //
    // `arm_receive_only_monitor` itself only returns once `StartComPrimitive`
    // has enqueued the registrant's `TxItem` -- the actual tier-2 registrant
    // insertion (and its `CopStatus(Executing)` emission) happens later, in
    // the poll task's own processing of that item (`cop_ctrl_cycles.rs`'s
    // own doc comment on this same registrant shape: "inserted directly as
    // tier-2 at creation and its owning poll task returns immediately").
    // Step 4 injects the shared frame as a single, non-retried event, so
    // this call is inlined (rather than using the shared harness helper) to
    // capture its `cop_handle` and poll `GetStatus` until the registrant is
    // confirmed live -- otherwise this step racing the poll task's own
    // registrant insertion could lose the frame for cll_b entirely, with no
    // later re-delivery to catch it. This race predates the `send_cop_status`
    // queueing fix (P2 backlog follow-up, `docs/implementation-notes.md`); that
    // fix's extra `resolve_queue_target` work on the same poll-task path
    // shifted timing enough to make the race reliably lose instead of
    // reliably win.
    let cll_b_monitor = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 0,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv, receive-only monitor) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    let mut monitor_ready = false;
    for _ in 0..200 {
        let status = client
            .get_status(vci_service_interface::GetStatusRequest {
                handle: Some(
                    vci_service_interface::get_status_request::Handle::CopHandle(cll_b_monitor),
                ),
            })
            .await
            .expect("get_status(COP) should succeed")
            .into_inner()
            .status;
        if matches!(
            status,
            Some(vci_service_interface::status_response::Status::CopStatus(s))
                if s == vci_service_interface::PduComPrimitiveStatus::PduCopstExecuting as i32
        ) {
            monitor_ready = true;
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    assert!(
        monitor_ready,
        "cll_b's receive-only monitor should reach PduCopstExecuting (registrant live) within \
         ~2s"
    );

    // Step 4: a single frame, routed to both CLLs by their own tables
    // (matching CP_CanRespUSDTId, distinct unique_resp_identifier), is
    // delivered to BOTH monitors independently -- proving both stayed live
    // and functional simultaneously.
    let shared_payload = vec![0x62, 0xF1, 0x91, 0x77];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &shared_payload),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events_a, 2000, |item| {
            matches!(&item.data, Some(event_item::Data::ResultData(result))
                if result.data_bytes == shared_payload && result.unique_resp_identifier == 1)
        })
        .await,
        "cll_a's monitor should still be live and observe the shared frame"
    );
    let cll_b_result = wait_for_result_data(&mut client, cll_b).await;
    assert_result_data(
        &cll_b_result,
        &0x7E8_u32.to_be_bytes(),
        &[],
        &shared_payload,
    );
    assert_eq!(
        cll_b_result.unique_resp_identifier, 2,
        "cll_b's monitor should independently observe the same shared frame"
    );

    drop(events_a);
    server.shutdown().await;
}

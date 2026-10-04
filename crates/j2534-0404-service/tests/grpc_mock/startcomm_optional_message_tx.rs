//! ADR-111 (fixing iso22900-2-conformance-audit.md finding A1-4): a CAN/J1850
//! `CoptStartcomm`'s optional request message (ISO 22900-2 §9.2.6.3.2 b),
//! Table 7 step 5) is transmitted -- and, when `NumReceiveCycles != 0`,
//! awaited for a response -- instead of being silently discarded. Reuses
//! `CoptStopcomm`'s one-shot transmit(+receive) machinery (`OneShotCommTx`,
//! ADR-085/ADR-087), but `cancellable: true` throughout (nothing has
//! committed CLL state yet at this point, unlike `CoptStopcomm`) and resolved
//! against `binding.resolved()` (Working when `temp_param_update` was set,
//! since -- unlike `CoptStopcomm` -- `CoptStartcomm`'s Temp binding is
//! genuinely pushed to hardware for this transaction).

use serial_test::serial;
use vci_service_interface::{
    CancelComPrimitiveRequest, ComPrimitiveCtrlData, ExpectedResponseData,
    StartComPrimitiveRequest, event_item, subscribe_event_request,
};

use crate::harness::*;

/// `CP_N_Bs` ComParam ID (ISO 15765-2 FlowControl wait timeout, ADR-046),
/// used by the temp-binding transmit-failure test to force a fast,
/// deterministic multi-frame ISO-TP timeout.
const CP_N_BS: u32 = 0x8046;

/// Waits for the next `PduCopstFinished` on an already-open event stream
/// (mirrors `stopcomm_data_tx.rs`'s helper of the same name).
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

/// Waits for the next `PduCllstCommStarted` on an already-open event stream.
async fn wait_for_cll_comm_started(
    events: &mut tonic::Streaming<vci_service_interface::EventNotification>,
) {
    assert!(
        wait_for_event(events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstCommStarted as i32
        ))
        .await,
        "expected a PduCllstCommStarted event"
    );
}

/// CAN, non-empty `cop_data`, `NumReceiveCycles = 0`: the optional message
/// transmits exactly once, no receive phase runs, and the CLL still reaches
/// `PDU_CLLST_COMM_STARTED` -- fire-and-forget, mirroring
/// `CoptSendrecv`/`CoptStopcomm`'s own `NumReceiveCycles == 0` convention
/// (ADR-058/ADR-085).
#[tokio::test]
#[serial]
async fn can_optional_message_zero_receive_cycles_transmits_and_reaches_comm_started() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let optional_message = vec![0x81, 0x01];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: optional_message.clone(),
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStartcomm, optional cop_data) should succeed");

    wait_for_cll_comm_started(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the optional CoptStartcomm message must transmit exactly once"
    );
    let mut expected = 0x7E0_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&optional_message);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    drop(events);
    server.shutdown().await;
}

/// CAN, non-empty `cop_data`, `NumReceiveCycles = 1`: a matching mock ECU
/// response is delivered as a `ResultData` result item attributed to this
/// `cop_handle`, and the CLL still reaches `PDU_CLLST_COMM_STARTED` once the
/// response arrives.
#[tokio::test]
#[serial]
async fn can_optional_message_matching_response_delivers_result_data_and_reaches_comm_started() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 3_000_000), // 3 s -- long enough to inject the response into.
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
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // A KWP2000-shaped StartCommunication request, expecting a positive
    // response (first payload byte 0xC1) delivered under acceptance_id 7.
    let optional_message = vec![0x81, 0x01];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: optional_message,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC1],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStartcomm, expected_response_array) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Genuinely still waiting: no PduCllstCommStarted yet.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstCommStarted as i32
        ))
        .await,
        "PduCllstCommStarted must not be emitted before the expected response is delivered"
    );

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0xC1, 0x01]),
        j2534_0404::ISO15765,
    );

    let mut received: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                received = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "the matching response should be delivered via ResultData"
    );
    let received = received.expect("captured");
    assert_eq!(received.acceptance_id, 7);

    wait_for_cll_comm_started(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// CAN, non-empty `cop_data`, `NumReceiveCycles = 1`, but the mock ECU never
/// sends a matching response: a `PduErrEvtRxTimeout` error event is emitted
/// (best-effort, per `wait_for_expected_response`'s existing behavior), but
/// -- per ISO 22900-2 §9.2.6.3.2 b)'s unconditional state-change sentence and
/// case d)'s SendRecv-equivalence -- the CLL still reaches
/// `PDU_CLLST_COMM_STARTED` and the COP still finishes. This is the key
/// spec-conformance assertion ADR-111/finding A1-4 exists for: an unanswered
/// optional message must not strand the CLL below COMM_STARTED.
#[tokio::test]
#[serial]
async fn can_optional_message_no_response_times_out_but_still_reaches_comm_started() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 100_000), // 100 ms window -- fast, deterministic timeout.
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
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let optional_message = vec![0x81, 0x01];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: optional_message,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC1],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStartcomm, expected_response_array) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // No response ever arrives: the P2Max window closes with a timeout, but
    // (unlike every OTHER failed-transmit/timeout path in handle_start_comm)
    // this must still reach COMM_STARTED.
    let mut saw_rx_timeout = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32
            ) {
                saw_rx_timeout = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CllStatus(status))
                    if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstCommStarted as i32
            )
        })
        .await,
        "the CLL should still reach PDU_CLLST_COMM_STARTED despite the unanswered optional message"
    );
    assert!(
        saw_rx_timeout,
        "an unanswered optional message should emit PduErrEvtRxTimeout"
    );

    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// CAN, non-empty `cop_data`, `NumReceiveCycles = -2` (IS-MULTIPLE): every
/// matching response from more than one ECU within the `CP_P2Max` window is
/// collected as its own `ResultData` result item (mirroring
/// `cop_ctrl_cycles.rs`'s `sendrecv_is_multiple_collects_all_matches_within_the_window`
/// for `CoptSendrecv`), and the COP still reaches `PDU_CLLST_COMM_STARTED`/
/// `PduCopstFinished` once the window closes.
#[tokio::test]
#[serial]
async fn can_optional_message_is_multiple_collects_all_matches_and_reaches_comm_started() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // A short 400 ms window keeps the test fast; it restarts after each
    // accepted response.
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 400_000),
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
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let optional_message = vec![0x22, 0xF1, 0x90];
    let cop = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: optional_message,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -2,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 5,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x62],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStartcomm, IS-MULTIPLE expected_response_array) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Two ECUs answer the same request inside the window.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E9, &[0x62, 0xF1, 0x90, 0x02]),
        j2534_0404::ISO15765,
    );

    // Both responses are accepted, then the window closes and the COP
    // reaches PDU_CLLST_COMM_STARTED/PduCopstFinished -- with no RxTimeout
    // error along the way.
    let mut accepted = 0;
    let mut saw_rx_timeout = false;
    let mut saw_comm_started = false;
    assert!(
        wait_for_event(&mut events, 3000, |item| {
            match &item.data {
                Some(event_item::Data::ResultData(result)) if result.acceptance_id == 5 => {
                    accepted += 1;
                }
                Some(event_item::Data::ErrorData(error))
                    if *error
                        == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32 =>
                {
                    saw_rx_timeout = true;
                }
                Some(event_item::Data::CllStatus(status))
                    if *status
                        == vci_service_interface::PduComLogicalLinkStatus::PduCllstCommStarted
                            as i32 =>
                {
                    saw_comm_started = true;
                }
                _ => {}
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
            ) && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop.cop_handle)
        })
        .await,
        "the COP should finish when the IS-MULTIPLE window closes"
    );
    assert_eq!(accepted, 2, "both ECU responses should be accepted");
    assert!(
        !saw_rx_timeout,
        "a window close with responses is not a receive timeout"
    );
    assert!(
        saw_comm_started,
        "the CLL should reach PDU_CLLST_COMM_STARTED once the IS-MULTIPLE window closes"
    );

    drop(events);
    server.shutdown().await;
}

/// J1850 (VPW), non-empty `cop_data`, `NumReceiveCycles = 0`: confirms the
/// new `tx` branch is not accidentally CAN-specific despite
/// `protocol_requires_init`'s CAN/J1850 pairing in the A1-4 audit finding --
/// the optional message transmits exactly once (with the service-constructed
/// 3-byte J1850 header, ADR-050) and the CLL still reaches
/// `PDU_CLLST_COMM_STARTED`, fire-and-forget.
#[tokio::test]
#[serial]
async fn j1850_optional_message_transmits_and_reaches_comm_started() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850VPW,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // ADR-050: cop_data is payload-only; the service constructs the 3-byte
    // J1850 header (format/target/source) from ComParams. No
    // CP_PhysReqFormatPriorityType/CP_PhysReqTargetAddr/NODE_ADDRESS were
    // set above, so the header uses this service's documented fallback
    // defaults (0x68 = standard OBD-II VPW priority byte / 0x10 target /
    // 0xF1 tester source).
    let optional_message = vec![0x01, 0x00];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: optional_message.clone(),
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStartcomm, optional cop_data) should succeed");

    wait_for_cll_comm_started(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    let mut expected = vec![0x68, 0x10, 0xF1];
    expected.extend_from_slice(&optional_message);
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the optional CoptStartcomm message must transmit exactly once"
    );
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::J1850VPW
    );
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    drop(events);
    server.shutdown().await;
}

/// `NumReceiveCycles == -1` (IS-CYCLIC) is rejected synchronously
/// (`INVALID_ARGUMENT`) for the optional `CoptStartcomm` message -- an "until
/// cancelled" receive can never let the CLL reach `PDU_CLLST_COMM_STARTED`,
/// mirroring `CoptStopcomm`'s identical rejection (ADR-087).
#[tokio::test]
#[serial]
async fn can_optional_message_num_receive_cycles_is_cyclic_rejected_synchronously() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x81, 0x01],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -1,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect_err(
            "start_com_primitive(CoptStartcomm, num_receive_cycles = -1) should be rejected",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("-1"), "{}", status.message());
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    // Nothing was enqueued: a follow-up plain CoptStartcomm still succeeds.
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "start_com_primitive(CoptStartcomm, empty cop_data) should succeed after the \
             rejected attempt",
        );
    wait_for_cll_comm_started(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// A `CancelComPrimitive` arriving while the optional message's receive phase
/// is genuinely still waiting for a matching response cancels the COP
/// (`PduCopstCancelled`) -- unlike `CoptStopcomm`'s analogous, always
/// non-cancellable receive phase (ADR-087), `CoptStartcomm`'s optional
/// message phase is fully cancellable (ADR-111): nothing has committed CLL
/// state yet at this point. The CLL must NOT reach `PDU_CLLST_COMM_STARTED`.
#[tokio::test]
#[serial]
async fn can_optional_message_cancel_during_receive_phase_cancels_before_comm_started() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 3_000_000), // 3 s -- long enough to cancel into.
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
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let start_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x81, 0x01],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC1],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStartcomm, expected_response_array) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Genuinely still waiting: no PduCllstCommStarted yet.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstCommStarted as i32
        ))
        .await,
        "PduCllstCommStarted must not be emitted before the expected response is delivered"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(start_cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == start_cop_handle.cop_handle))
        .await,
        "CancelComPrimitive during the optional message's receive phase should cancel it \
         (ADR-111: fully cancellable, unlike CoptStopcomm's analogous phase)"
    );

    // The CLL must never reach COMM_STARTED for the cancelled attempt.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstCommStarted as i32
        ))
        .await,
        "a cancelled CoptStartcomm must not reach PDU_CLLST_COMM_STARTED"
    );

    drop(events);
    server.shutdown().await;
}

/// `temp_param_update = 1` (Temp binding) with a transmit failure: the
/// transient hardware apply is reverted to the live Active set, the CLL does
/// NOT reach `PDU_CLLST_COMM_STARTED`, and -- unlike a K-line init failure --
/// no `PduErrEvtInitError` is ever emitted (that event is specific to the
/// K-line init sequence, which this path never runs).
///
/// Forces a genuine transmit failure deterministically via a software-ISO-TP
/// multi-frame send whose FlowControl never arrives (`CP_N_Bs` timeout,
/// mirroring `stopcomm_data_tx.rs`'s multi-frame ISO-TP coverage) -- the mock
/// adapter has no direct "fail the next write" backdoor, so this is the only
/// existing, established way to produce a genuine `TxFailure::Event` in this
/// test suite.
#[tokio::test]
#[serial]
async fn can_optional_message_temp_param_update_transmit_failure_reverts_hardware() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    // LOOPBACK is a hardware SET_CONFIG param (not PDU_PC_BUSTYPE-class, so
    // Working may differ from Active without tripping the temp guard) --
    // staged in Working only (never promoted), so it distinguishes "the temp
    // apply pushed Working" from "the revert restored live Active".
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000), (j2534_0404::LOOPBACK, 0)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // Stage Working only (never promoted): LOOPBACK=1 (to observe the
    // apply/revert bracket) and a short CP_N_Bs so the never-answered
    // FlowControl wait times out quickly and deterministically.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::LOOPBACK, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_N_BS, 100_000).await;

    let baseline_set_config = server.backdoor.set_config_count();

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // 20-byte payload -> FirstFrame + ConsecutiveFrame(s), forcing a genuine
    // FlowControl wait (same segmentation `stopcomm_data_tx.rs`'s
    // `stopcomm_multiframe_cancel_during_transmit_does_not_abort` pins) --
    // no FlowControl is ever injected, so the wait times out.
    let optional_message: Vec<u8> = (1..=20).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: optional_message,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStartcomm, temp_param_update=1) should succeed");

    // The FirstFrame goes out under the temp (Working) binding.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut saw_init_error = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == vci_service_interface::PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "the failed CoptStartcomm should still finish (PduCopstFinished), not hang"
    );
    assert!(
        !saw_init_error,
        "a failed optional-message transmit must not emit PduErrEvtInitError -- that event is \
         specific to the K-line init sequence, which this path never runs"
    );

    // The CLL never reached COMM_STARTED: the transmit never reached the bus.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstCommStarted as i32
        ))
        .await,
        "a failed optional-message transmit must not reach PDU_CLLST_COMM_STARTED"
    );

    // Exactly two SET_CONFIG calls: apply Working (LOOPBACK=1), then revert
    // to the live Active set (LOOPBACK=0) on the transmit failure.
    assert_eq!(server.backdoor.set_config_count(), baseline_set_config + 2);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::LOOPBACK),
        0,
        "hardware must be reverted to the live Active value (LOOPBACK=0), not left on the \
         temp Working value (LOOPBACK=1)"
    );

    drop(events);
    server.shutdown().await;
}

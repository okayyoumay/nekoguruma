//! ADR-088: `CP_TesterPresentReqRsp = 1` discards the ECU's tester-present
//! response instead of delivering it to the client as an unsolicited
//! `ResultData`.

use serial_test::serial;
use vci_service_interface::{
    ComPrimitiveCtrlData, ConnectComLogicalLinkRequest, ExpectedResponseData,
    StartComPrimitiveRequest, event_item, subscribe_event_request,
    vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// Waits for the next `PduCopstFinished` on an already-open event stream.
/// The stream must be subscribed BEFORE the COP being waited on is started,
/// so its `PduCopstFinished` cannot be missed.
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

async fn start_comm(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
}

/// Starts a `CoptSendrecv` with an explicit `expected_response` (`mask_data:
/// [0xFF]`, `pattern_data: [0x7E]`, `acceptance_id: 77`) and one receive
/// cycle, so a `MatchProbe` is left actively waiting on `cll_handle` --
/// mirrors `rc_handling.rs`'s `start_send_recv`, duplicated locally so this
/// file's discard-vs-pending-COP-claim test is not coupled to another test
/// module's helper.
async fn start_pending_sendrecv(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) {
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
                    acceptance_id: 77,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x7E],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");
}

/// Same as `start_pending_sendrecv`, but with a VACUOUS `expected_response`
/// (empty `mask_data`/`pattern_data`, matching any payload) -- ADR-100's
/// `bind_frame` field-bug regression test below needs a registrant whose own
/// claim on a frame is vacuous, unlike `start_pending_sendrecv`'s
/// specifically-expected `[0xFF]`/`[0x7E]` descriptor.
async fn start_vacuous_pending_sendrecv(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) {
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
                    acceptance_id: 55,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");
}

/// Creates an ISO15765 CLL, connects it, and stages a UniqueRespIdTable entry
/// routing CAN ID `0x7E8` (`CP_CanRespUSDTId`) with `CP_CanPhysReqId =
/// 0x7E0` -- mirrors `response_distribution.rs`'s routing setup -- plus a
/// non-empty `CP_TesterPresentMessage` so `CoptStartcomm` actually arms
/// tester-present (mode 0, the default `CP_TesterPresentSendType`, arms as
/// soon as `CoptStartcomm` completes, with no interval to wait out). Callers
/// stage any additional `CP_TesterPresentReqRsp`/`ExpPosResp`/`ExpNegResp`
/// ComParams and promote once before arming.
///
/// Also widens `CP_P2Max` to a generous 5 s (ADR-088 amendment): tester-present
/// discard is window-scoped to `fired_at + CP_P2Max` for both
/// `CP_TesterPresentSendType` values (mode 0 window-scoped as of this diff,
/// same as mode 1 already was), so a tight default window could flake under
/// test-harness scheduling delays.
async fn create_routed_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
) -> vci_service_interface::ComLogicalLinkHandle {
    let cll_handle = create_and_connect_cll(
        client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_unique_resp_table(
        client,
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
    set_com_param_bytes(client, cll_handle, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    // A zero CP_TesterPresentTime resolves tester-present as disabled even
    // with a message configured, leaving TesterPresentState::None -- a
    // non-zero interval is required for CoptStartcomm to actually arm
    // (TesterPresentState::Armed).
    set_com_param_unum32(client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(client, cll_handle, j2534_0404::P2_MAX, 5_000_000).await;
    cll_handle
}

/// `CP_TesterPresentReqRsp = 1` with a configured `CP_TesterPresentExpPosResp
/// = [0x7E]`: once tester-present is armed, an unclaimed frame whose payload
/// starts with `0x7E` is the ECU's tester-present acknowledgement and must be
/// discarded, never reaching the client as `ResultData` (ADR-088).
#[tokio::test]
#[serial]
async fn reqrsp_1_discards_positive_response_prefix_match() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a frame matching the configured ExpPosResp prefix should have been discarded, not \
         delivered as ResultData"
    );

    drop(events);
    server.shutdown().await;
}

/// Same as `reqrsp_1_discards_positive_response_prefix_match`, but with
/// `CP_TesterPresentSendType = 1` (idle-triggered) instead of the default
/// mode 0 (periodic) -- both resolve to the same `TesterPresentState::Armed`
/// variant (`resolved.send_type` is the mode discriminator, not the enum
/// shape), but every other test in this file only ever exercises the default
/// mode-0 arm via `create_routed_cll`'s default send type. Per ADR-084, mode
/// 1's `CoptStartcomm` itself sends the first tester-present frame
/// immediately, synchronously, before completing, so tester-present is
/// already armed by the time `wait_for_cop_finished` returns -- no
/// additional wait for the (deliberately huge, 5s) idle interval is needed
/// (ADR-088). Relies on `create_routed_cll`'s generous `CP_P2Max` (5 s):
/// discard is window-scoped to `fired_at + CP_P2Max` (ADR-088 amendment),
/// and the injection below happens immediately after arming -- well inside
/// that window.
#[tokio::test]
#[serial]
async fn reqrsp_1_discards_positive_response_prefix_match_mode_1_idle() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a frame matching the configured ExpPosResp prefix should have been discarded while \
         tester-present is armed in mode 1 (Idle), not delivered as ResultData"
    );

    drop(events);
    server.shutdown().await;
}

/// Same as `reqrsp_1_discards_positive_response_prefix_match`, but for
/// `CP_TesterPresentExpNegResp = [0x7F, 0x3E]` (the UDS default) -- an
/// ordinary NRC byte (not 0x78/0x21/0x23, which would instead be claimed by
/// the pending-RC branch, an accepted residual ADR-088 already documents)
/// still results in discard.
#[tokio::test]
#[serial]
async fn reqrsp_1_discards_negative_response_prefix_match() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_NEG_RESP,
        vec![0x7F, 0x3E],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x3E, 0x22]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a frame matching the configured ExpNegResp prefix should have been discarded, not \
         delivered as ResultData"
    );

    drop(events);
    server.shutdown().await;
}

/// `CP_TesterPresentReqRsp = 0` (its default): a configured
/// `CP_TesterPresentExpPosResp` alone must not cause any discard -- the
/// matching frame is delivered as an ordinary unsolicited `ResultData`.
#[tokio::test]
#[serial]
async fn reqrsp_0_does_not_discard() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    // CP_TesterPresentReqRsp explicitly left at its default (0).
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 0).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the frame below
    // delivered under the new unbound-discard model (CoptStartcomm above has
    // already fully finished, so this shared channel's poll task is free).
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x7E, 0x00];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::ISO15765,
    );

    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "CP_TesterPresentReqRsp = 0 should not discard a frame matching ExpPosResp's prefix"
    );
    assert_eq!(delivered.expect("captured").data_bytes, payload);

    drop(events);
    server.shutdown().await;
}

/// A CLL that never armed tester-present (no `CoptStartcomm` issued --
/// `TesterPresentState::None`) must not discard anything even though
/// `CP_TesterPresentReqRsp = 1` and `CP_TesterPresentExpPosResp` are both
/// configured -- guards the hazard of a CLL that merely defaulted/left
/// `CP_TesterPresentReqRsp` at `1` without ever configuring tester-present.
#[tokio::test]
#[serial]
async fn reqrsp_1_not_armed_does_not_discard() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Deliberately no CoptStartcomm -- tester-present is never armed.

    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the frame below
    // delivered under the new unbound-discard model.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x7E, 0x00];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::ISO15765,
    );

    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "a CLL whose tester-present was never armed must not discard a frame matching \
         ExpPosResp's prefix, even with CP_TesterPresentReqRsp = 1 configured"
    );
    assert_eq!(delivered.expect("captured").data_bytes, payload);

    drop(events);
    server.shutdown().await;
}

/// A pending COP's own claim always wins over the tester-present discard: a
/// frame that matches both a waiting `CoptSendrecv`'s `expected_response`
/// pattern AND the configured `ExpPosResp` prefix is attributed to the
/// waiting COP (delivered with its `acceptance_id`), not silently discarded
/// (ADR-088's "a pending COP's own claim always wins" design choice).
#[tokio::test]
#[serial]
async fn reqrsp_1_pending_cop_claim_wins_over_discard() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    start_pending_sendrecv(&mut client, cll_handle).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "a frame matching a pending COP's own expected_response should still be delivered, not \
         discarded, even though it also matches the tester-present ExpPosResp prefix"
    );
    assert_eq!(
        delivered.expect("captured").acceptance_id,
        77,
        "the delivered match should be attributed to the waiting COP"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-100 Decision §3's field-bug regression, end to end: contrast case for
/// `reqrsp_1_pending_cop_claim_wins_over_discard` above. When the waiting
/// COP's OWN `expected_response` is vacuous (empty mask/pattern -- matches
/// anything, e.g. a `CoptSendrecv` misconfigured the way the field report
/// described), the tester-present discard now wins instead: the frame is
/// bound to tester-present's own reply signature (step 3, ahead of a vacuous
/// tier-1 claim at step 4) and discarded outright, never delivered as
/// `ResultData` to the vacuous COP or to anyone else. Pre-ADR-100, the
/// vacuous descriptor's match-anything semantics let the old `MatchProbe`
/// (which ran before the tester-present check at all) claim the frame first
/// -- this is the exact capture the field report was about.
#[tokio::test]
#[serial]
async fn reqrsp_1_vacuous_cop_claim_loses_to_discard() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    // ADR-137: a plain ISO15765 CLL defaults CP_TesterPresentHandling to 0
    // (ISO_15765_4 = 0), so it must be explicitly promoted to 1 for
    // CoptStartcomm below to arm tester-present at all.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    start_vacuous_pending_sendrecv(&mut client, cll_handle).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a frame matching the tester-present ExpPosResp prefix must be discarded, not delivered \
         to a vacuous-descriptor waiting COP (ADR-100 field-bug regression) nor as unsolicited \
         ResultData"
    );

    drop(events);
    server.shutdown().await;
}

/// `CP_TesterPresentReqRsp = 1` but neither `ExpPosResp` nor `ExpNegResp`
/// configured (both left empty): an empty configured pattern never matches,
/// so nothing is discarded -- the frame is delivered normally.
#[tokio::test]
#[serial]
async fn reqrsp_1_empty_patterns_never_match() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    // CP_TesterPresentExpPosResp/ExpNegResp deliberately left unset (empty).
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the frame below
    // delivered under the new unbound-discard model.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x7E, 0x00];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::ISO15765,
    );

    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "with no ExpPosResp/ExpNegResp configured, no frame should ever be discarded"
    );
    assert_eq!(delivered.expect("captured").data_bytes, payload);

    drop(events);
    server.shutdown().await;
}

/// Creates an ISO15765 CLL, connects it, and stages TWO `UniqueRespIdTable`
/// entries -- `unique_resp_identifier: 1` (CAN ID `0x7E8`/phys-req `0x7E0`)
/// and `unique_resp_identifier: 2` (CAN ID `0x7E9`/phys-req `0x7E1`) -- with
/// no functional request id configured anywhere, so live Active addressing
/// resolves to CAN-family *physical* (ADR-088 amendment). Otherwise
/// identical to `create_routed_cll`: non-empty `CP_TesterPresentMessage`,
/// non-zero `CP_TesterPresentTime`, and a generous `CP_P2Max`. Shared by the
/// physical-addressing discard-restriction tests below.
async fn create_two_entry_routed_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
) -> vci_service_interface::ComLogicalLinkHandle {
    let cll_handle = create_and_connect_cll(
        client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_unique_resp_table(
        client,
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
    set_com_param_bytes(client, cll_handle, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    // See create_routed_cll: a non-zero interval is required for
    // CoptStartcomm to actually arm tester-present.
    set_com_param_unum32(client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(client, cll_handle, j2534_0404::P2_MAX, 5_000_000).await;
    cll_handle
}

/// ADR-088 amendment: physical addressing restricts the discard to the
/// `unique_resp_identifier` tester-present is actually addressed to (the
/// FIRST `UniqueRespIdTable` entry, per `resolve_tester_present`'s own
/// addressing rule) -- unlike `ExpectedResponse.unique_resp_ids`, which has
/// no such restriction. Stages a SECOND `UniqueRespIdTable` entry
/// (`unique_resp_identifier: 2`, CAN ID `0x7E9`) alongside the usual first
/// entry, and injects the discardable frame on the SECOND entry's CAN ID
/// rather than the first's -- confirming discard does NOT apply to a frame
/// from a routed ECU other than tester-present's own physical target, unlike
/// the original (corrected) implementation that discarded regardless of
/// routed ECU.
#[tokio::test]
#[serial]
async fn reqrsp_1_physical_addressing_does_not_discard_other_unique_resp_id() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_two_entry_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the frame below
    // delivered under the new unbound-discard model.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // The SECOND entry's CAN ID (unique_resp_identifier 2), not the first's
    // (tester-present's own physical target).
    let payload = vec![0x7E, 0x00];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E9, &payload),
        j2534_0404::ISO15765,
    );

    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "a frame matching the configured ExpPosResp prefix, but from a routed ECU other than \
         tester-present's own physical target, should be delivered, not discarded"
    );
    assert_eq!(delivered.expect("captured").data_bytes, payload);

    drop(events);
    server.shutdown().await;
}

/// Contrast case for `reqrsp_1_physical_addressing_does_not_discard_other_unique_resp_id`:
/// same 2-entry setup, but the discardable frame arrives on the FIRST
/// entry's own CAN ID (`0x7E8`, tester-present's actual physical target) --
/// confirms the restriction is a genuine *restriction* (still discards for
/// the right target), not an accidental "resp_uid always None" regression.
#[tokio::test]
#[serial]
async fn reqrsp_1_physical_addressing_discards_the_actual_target() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_two_entry_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // The FIRST entry's CAN ID (unique_resp_identifier 1) -- tester-present's
    // own physical target.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a frame matching the configured ExpPosResp prefix, from tester-present's own physical \
         target (the first UniqueRespIdTable entry), should still be discarded"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-088 amendment, core new behavior: mode 1 (idle-triggered)
/// tester-present discard is window-scoped to `fired_at + CP_P2Max`. Sets
/// `CP_P2Max` to a short, deterministic 50 ms (overriding
/// `create_routed_cll`'s generous 5 s default for this one test), arms
/// tester-present, then sleeps comfortably past the window (150 ms) before
/// injecting a frame matching `ExpPosResp`'s prefix -- confirms the frame is
/// delivered as an ordinary `ResultData`, not discarded, since it arrived
/// long after the actual send and is therefore unrelated to it.
#[tokio::test]
#[serial]
async fn reqrsp_1_mode_1_window_expired_does_not_discard() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    // Overrides create_routed_cll's generous 5 s CP_P2Max with a short,
    // deterministic 50 ms window for this test.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 50_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the frame below
    // delivered under the new unbound-discard model.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // Comfortably past the 50 ms CP_P2Max window.
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    let payload = vec![0x7E, 0x00];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::ISO15765,
    );

    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "a frame arriving after the CP_P2Max discard window has expired should be delivered, \
         not discarded, even though it matches the configured ExpPosResp prefix"
    );
    assert_eq!(delivered.expect("captured").data_bytes, payload);

    drop(events);
    server.shutdown().await;
}

/// This diff (§7 of the design brief): mode 0 (`CP_TesterPresentSendType =
/// 0`, periodic, left at its default here) now goes through the same
/// `discard_until`-windowed `build_cll_rx_entries` arm as mode 1, instead of
/// the pre-diff armed-wide (unwindowed) `Periodic` discard that never
/// expired for as long as tester-present stayed armed. Mirrors
/// `reqrsp_1_mode_1_window_expired_does_not_discard`'s structure exactly,
/// substituting the default mode 0 send type for an explicit mode-1
/// override -- proving the windowing is no longer mode-1-specific.
#[tokio::test]
#[serial]
async fn reqrsp_1_mode_0_window_expired_does_not_discard() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    // CP_TesterPresentSendType is left at its default (0, periodic).
    // Overrides create_routed_cll's generous 5 s CP_P2Max with a short,
    // deterministic 50 ms window for this test.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 50_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the frame below
    // delivered under the new unbound-discard model.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // Comfortably past the 50 ms CP_P2Max window.
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    let payload = vec![0x7E, 0x00];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::ISO15765,
    );

    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "a frame arriving after mode 0's CP_P2Max discard window has expired should be \
         delivered, not discarded, even though it matches the configured ExpPosResp prefix -- \
         mode 0 is no longer armed-wide (unwindowed)"
    );
    assert_eq!(delivered.expect("captured").data_bytes, payload);

    drop(events);
    server.shutdown().await;
}

/// ADR-088 amendment: `resp_uid` is `None` (no restriction) when
/// tester-present is resolved as functional (broadcast) addressing --
/// every ECU answering a functional tester-present is a legitimate reply.
/// Configures functional addressing (`CP_RequestAddrMode = 2`,
/// `CP_CanFuncReqId = 0x7DF`, mirroring `tester_present_send_type.rs`)
/// alongside a 2-entry `UniqueRespIdTable`, arms tester-present, then
/// injects discardable frames on BOTH entries' CAN IDs -- confirming BOTH
/// are discarded, not just the first. Widens `CP_P2Max` to a generous 5 s
/// (mirroring `create_routed_cll`/`create_two_entry_routed_cll`, ADR-088
/// amendment): mode 0's discard is window-scoped to `fired_at + CP_P2Max`
/// as of this diff (§7), same as mode 1 already was, so this test would
/// otherwise be newly sensitive to the default preset's much tighter
/// 50 ms `CP_P2Max` under load.
#[tokio::test]
#[serial]
async fn reqrsp_1_functional_addressing_discards_any_routed_ecu() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_unique_resp_table(
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
    // Functional addressing (mirrors tester_present_send_type.rs): the
    // UniqueRespIdTable above is still consulted for RX routing, but
    // ignored for TX addressing (ADR-054) -- resolve_can_addressing resolves
    // `functional: true` from these two ComParams alone.
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 5_000_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E9, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "under functional addressing, a frame matching the configured ExpPosResp prefix should \
         be discarded regardless of which routed unique_resp_identifier it came from"
    );

    drop(events);
    server.shutdown().await;
}

/// This diff (§6 of the design brief: mode 0 now re-arms via `CoptUpdateparam`
/// exactly like mode 1, since mode 0 is no longer hardware-driven and so has
/// no hardware session to keep stable) reverses this test's pre-diff
/// premise: a `SetUniqueRespIdTable` + `CoptUpdateparam` that reorders the
/// table so a DIFFERENT entry is now first changes which entry's
/// `CP_CanPhysReqId` `resolve_tester_present` builds the outgoing request
/// from (`resolved.data`), so `same_wire_behavior` now detects a real
/// difference and re-arms -- immediately sending a new frame and refreezing
/// `resolved.target_can_ids` to the NEW first entry's response CAN ID, NOT
/// leaving it pinned to the original entry the way mode 0 used to
/// (pre-this-diff) when its hardware-driven `PassThruStartPeriodicMsg`
/// session had no way to be updated short of a stop/restart nobody
/// requested.
///
/// Arms mode-0 tester-present against the two-entry table (entry 1 =
/// `0x7E8`/phys `0x7E0` is first, so `resolved.target_can_ids` freezes to
/// `0x7E8`), then reorders the table so entry 2 (`0x7E9`/phys `0x7E1`) is
/// now first. Confirms a second arm-time-style send goes out (re-arm), and
/// that afterward a frame on `0x7E9` (the NEW target) is discarded.
///
/// ADR-137 fourth Codex-review fix (round-4 restructure) updated this test's
/// OTHER assertion: a frame on `0x7E8` (the OLD target) is now ALSO
/// discarded, not delivered, as long as entry 1's own arm-time-send window
/// (generous 5s `CP_P2Max`, `create_two_entry_routed_cll`) is still open --
/// the opposite of this test's pre-round-4 assertion, which relied on the
/// rounds 1-3 single-slot `TesterPresentState::Armed::discard_until` being
/// silently overwritten by the reorder-triggered re-arm's own fresh window.
/// Under the round-4 list model (`LogicalLinkState.open_tp_discards`), that
/// arm-time send's window is an independently elicited, still-open discard
/// candidate exactly like the fresh re-arm's own window -- a delayed ECU
/// reply to the ACTUAL frame this CLL sent to `0x7E0` before the reorder is
/// still tester-present noise, not client traffic, regardless of what the
/// CLL is newly configured to target. (Eventual expiry of an old window like
/// this one is covered generically by `reqrsp_1_mode_0_window_expired_does_not_discard`
/// and `tester_present_send_type.rs`'s own round-4 expiry tests, not
/// re-proven here.)
#[tokio::test]
#[serial]
async fn reqrsp_1_mode_0_rearms_and_retargets_on_table_reorder_that_changes_tx_data() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_two_entry_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    // ADR-137: a plain ISO15765 CLL defaults CP_TesterPresentHandling to 0
    // (ISO_15765_4 = 0), so it must be explicitly promoted to 1 for
    // CoptStartcomm below to arm tester-present at all.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Arms mode-0 tester-present: entry 1 (0x7E8, phys 0x7E0) is first, so
    // resolved.target_can_ids freezes to Some(0x7E8). ADR-084 (extended to
    // mode 0 by this diff) means the arm-time send already went out.
    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Reorder the live table: entry 2 (0x7E9, phys 0x7E1) is now first. Its
    // CP_CanPhysReqId (0x7E1) differs from entry 1's (0x7E0), so
    // resolved.data changes and this CoptUpdateparam DOES re-arm (§6 of this
    // diff).
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E9),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E1),
                ],
            ),
            ecu_entry(
                1,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
        ],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    // The re-arm's own immediate send should have gone out, addressed to the
    // NEW first entry's request CAN ID (0x7E1).
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let mut expected = 0x7E1_u32.to_be_bytes().to_vec();
    expected.push(0x3E);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected,
        "the re-arm should have sent a new tester-present frame addressed via the new first \
         entry's CP_CanPhysReqId"
    );
    // Entry 2's CAN ID (0x7E9) -- the NEW frozen target -- should now be
    // discarded.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E9, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "entry 2's frame should now be discarded -- it is the newly re-armed tester-present \
         target after the reorder-triggered re-arm"
    );

    // Entry 1's CAN ID (0x7E8) -- the OLD target -- should ALSO still be
    // discarded (ADR-137 fourth Codex-review fix / round-4 restructure): the
    // arm-time send's own window (generous 5s CP_P2Max, still open) is an
    // independently elicited discard candidate in
    // `LogicalLinkState.open_tp_discards`, not a single slot the
    // reorder-triggered re-arm's own write-back silently overwrote.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "entry 1's frame must still be discarded after the reorder-triggered re-arm -- it was \
         the target of the arm-time send, whose own still-open discard window is an \
         independently elicited entry in LogicalLinkState.open_tp_discards (ADR-137 fourth \
         Codex-review fix / round-4 restructure), not a single slot the fresh re-arm silently \
         overwrites"
    );

    drop(events);
    server.shutdown().await;
}

/// This diff (§6): unlike the reorder test above (which changes
/// `CP_CanPhysReqId` and so DOES re-arm), a `CoptUpdateparam` that swaps only
/// the RESPONSE addressing (`CP_CanRespUSDTId`) between two entries, leaving
/// each entry's own `CP_CanPhysReqId` (and so the transmitted bytes)
/// unchanged, still does NOT re-arm mode 0 -- `same_wire_behavior` (the
/// ADR-084 re-arm gate, shared by both modes as of this diff) compares
/// `data`, not `target_can_ids`. Mirrors
/// `reqrsp_1_mode_1_target_can_id_survives_response_id_only_updateparam`'s
/// structure exactly, substituting mode 0 for mode 1, to prove the frozen-
/// target-on-no-rearm behavior is now identical for both modes (the
/// asymmetry this test's pre-diff version pinned -- mode 0 froze because it
/// COULD NOT re-arm at all, mode 1 froze because this specific update
/// didn't qualify -- no longer exists; both freeze for the same, shared
/// reason).
///
/// Arms mode-0 tester-present against the two-entry table (entry 1 =
/// `0x7E8`/phys `0x7E0` is first, so `resolved.target_can_ids` freezes to
/// `0x7E8`), then swaps only the two entries' `CP_CanRespUSDTId` values
/// (entry 1 now declares `0x7E9`, entry 2 now declares `0x7E8`) while each
/// entry keeps its OWN original `CP_CanPhysReqId` -- so `resolved.data`
/// (built from the first entry's `CP_CanPhysReqId`, still `0x7E0`) is
/// unchanged and the re-arm gate does not fire. Confirms: `0x7E8` (the
/// frozen target, now labelled entry 2's response ID) is still discarded;
/// `0x7E9` (now entry 1's response ID, but never the actual resolved
/// target) is delivered, not discarded.
#[tokio::test]
#[serial]
async fn reqrsp_1_mode_0_target_can_id_survives_response_id_only_updateparam() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_two_entry_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    // ADR-137: a plain ISO15765 CLL defaults CP_TesterPresentHandling to 0
    // (ISO_15765_4 = 0), so it must be explicitly promoted to 1 for
    // CoptStartcomm below to arm tester-present at all.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Arms mode-0 tester-present: entry 1 (0x7E8, phys 0x7E0) is first, so
    // resolved.target_can_ids freezes to Some(0x7E8). ADR-084 (extended to
    // mode 0 by this diff) means the arm-time send already went out.
    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Swap ONLY the response IDs between the two entries -- each entry keeps
    // its own CP_CanPhysReqId, so resolved.data (built from the first
    // entry's CP_CanPhysReqId, still 0x7E0) is unchanged and this
    // CoptUpdateparam does not re-arm.
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![
            ecu_entry(
                1,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E9),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E1),
                ],
            ),
        ],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;

    // No re-arm should have happened -- the arm-time send from above remains
    // the only frame on the wire.
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a response-id-only CoptUpdateparam must not re-arm mode 0 (resolved.data is unchanged)"
    );
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps 0x7E9's frame
    // (below) delivered under the new unbound-discard model -- tester-
    // present's own discard (rank 3) still outranks it for 0x7E8's frame.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // 0x7E8 -- the frozen, true tester-present target, now labelled entry
    // 2's response ID -- should still be discarded.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "0x7E8 should still be discarded -- it is the frozen tester-present target's own CAN \
         ID, regardless of the later response-id-only CoptUpdateparam"
    );

    // 0x7E9 -- never the resolved target, now labelled entry 1's response
    // ID -- should be delivered, not discarded.
    let payload = vec![0x7E, 0x00];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E9, &payload),
        j2534_0404::ISO15765,
    );
    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "0x7E9 should be delivered, not discarded -- it was never the resolved tester-present \
         target, even though it is now labelled entry 1's response ID"
    );
    assert_eq!(delivered.expect("captured").data_bytes, payload);

    drop(events);
    server.shutdown().await;
}

/// ADR-088 amendment, second Codex review round: mode 1 (`Idle`) has the
/// SAME live-vs-armed divergence mode 0 needed fixing for, via a different
/// path. `same_wire_behavior` (the ADR-084 re-arm gate) compares `data`
/// (built from `CP_CanPhysReqId`, the TX/request addressing) but not
/// `target_can_ids` -- a `CoptUpdateparam` that swaps only the RESPONSE
/// addressing (`CP_CanRespUSDTId`) between two entries, leaving each
/// entry's own `CP_CanPhysReqId` (and so the transmitted bytes) unchanged,
/// does not re-arm. `dispatch_due_tester_present`'s own per-tick send
/// never re-resolves `resolved`/`framed_data` between arms either, so every
/// send inside the still-open `CP_P2Max` window genuinely went out against
/// whatever `resolved.target_can_ids` says -- recomputing the discard
/// target live from the CURRENT table (this test's fix removes exactly that
/// recompute) would drift the moment this swap lands, discarding the wrong
/// CAN ID for a response to an already-sent, unchanged request.
///
/// Arms mode-1 tester-present against the two-entry table (entry 1 =
/// `0x7E8`/phys `0x7E0` is first, so the resolved target freezes to
/// `0x7E8`), then swaps only the two entries' `CP_CanRespUSDTId` values
/// (entry 1 now declares `0x7E9`, entry 2 now declares `0x7E8`) while each
/// entry keeps its OWN original `CP_CanPhysReqId` -- so `resolved.data`
/// (built from the first entry's `CP_CanPhysReqId`, still `0x7E0`) is
/// unchanged and `handle_update_param`'s re-arm gate does not fire.
/// Confirms: `0x7E8` (the frozen target, now labelled entry 2's response
/// ID) is still discarded; `0x7E9` (now entry 1's response ID, but never
/// the actual resolved target) is delivered, not discarded.
#[tokio::test]
#[serial]
async fn reqrsp_1_mode_1_target_can_id_survives_response_id_only_updateparam() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_two_entry_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    // ADR-137: a plain ISO15765 CLL defaults CP_TesterPresentHandling to 0
    // (ISO_15765_4 = 0), so it must be explicitly promoted to 1 for
    // CoptStartcomm below to arm tester-present at all.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Arms mode-1 tester-present: entry 1 (0x7E8, phys 0x7E0) is first, so
    // resolved.target_can_ids freezes to Some(0x7E8). ADR-084's immediate
    // arm-time send means discard_until is already set by the time
    // wait_for_cop_finished returns.
    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // Swap ONLY the response IDs between the two entries -- each entry
    // keeps its own CP_CanPhysReqId, so resolved.data (built from the first
    // entry's CP_CanPhysReqId, still 0x7E0) is unchanged and this
    // CoptUpdateparam does not re-arm (same_wire_behavior compares `data`,
    // not target_can_ids).
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![
            ecu_entry(
                1,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E9),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E1),
                ],
            ),
        ],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps 0x7E9's frame
    // (below) delivered under the new unbound-discard model -- tester-
    // present's own discard (rank 3) still outranks it for 0x7E8's frame.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // 0x7E8 -- the frozen, true tester-present target, now labelled entry
    // 2's response ID -- should still be discarded.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "0x7E8 should still be discarded -- it is the frozen tester-present target's own CAN \
         ID, regardless of the later response-id-only CoptUpdateparam"
    );

    // 0x7E9 -- never the resolved target, now labelled entry 1's response
    // ID -- should be delivered, not discarded.
    let payload = vec![0x7E, 0x00];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E9, &payload),
        j2534_0404::ISO15765,
    );
    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "0x7E9 should be delivered, not discarded -- it was never the resolved tester-present \
         target, even though it is now labelled entry 1's response ID"
    );
    assert_eq!(delivered.expect("captured").data_bytes, payload);

    drop(events);
    server.shutdown().await;
}

/// Issues `CoptStartcomm` with `temp_param_update = 1` -- shared by
/// `reqrsp_1_mode_1_p2max_from_active_not_temp_working_binding` below.
async fn start_comm_temp_param_update(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
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
}

/// ADR-088 amendment, fourth Codex review round: `CP_P2Max` for mode 1's
/// discard window must come from the SAME Active snapshot tester-present
/// itself is resolved from, never `binding.resolved()` -- under
/// `temp_param_update = 1`, `binding` is `ParamBinding::Temp { effective }`,
/// the transient Working snapshot for the init transaction (already
/// reverted on hardware by the time tester-present arms), while
/// tester-present is always resolved from call-time Active regardless of
/// `temp_param_update` (ADR-067 claim 8). Stages a much SHORTER `CP_P2Max`
/// in Working only (never promoted) immediately before a
/// `temp_param_update=1` `CoptStartcomm`, while Active keeps the generous
/// value `create_routed_cll` already promoted -- confirms the discard
/// window is sized from Active (long), not the transient Working value
/// (short): injecting a matching frame well past the short Working value,
/// but still within the Active one, is still discarded.
#[tokio::test]
#[serial]
async fn reqrsp_1_mode_1_p2max_from_active_not_temp_working_binding() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    // Promotes create_routed_cll's generous CP_P2Max (5s) to Active, along
    // with everything else staged so far.
    promote_via_update_param(&mut client, cll_handle).await;

    // Stage a much SHORTER CP_P2Max in Working ONLY -- never promoted, so
    // Active stays at 5s. If the fix regressed to reading `binding.resolved()`
    // (Working, under temp_param_update=1), the discard window would close
    // after this short value instead.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 50_000).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Arms mode-1 tester-present via a temp_param_update=1 CoptStartcomm --
    // ADR-084's immediate arm-time send means discard_until is already set
    // by the time wait_for_cop_finished returns.
    start_comm_temp_param_update(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // Comfortably past the 50ms staged-Working value, but nowhere near the
    // 5s Active value.
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "the discard window should still be open 150ms after arm -- it must be sized from \
         Active's 5s CP_P2Max, not the transient Working value (50ms) temp_param_update=1 \
         staged for the init transaction only"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-088 second amendment (Codex review, PR #97 seventh round): a CLL
/// armed under `CP_TesterPresentReqRsp = 0` opens NO discard window at all
/// (`DiscardWindow` is only constructed when the send both succeeded AND
/// `expects_response` was `true` at that same send instant). A later
/// `CoptUpdateparam` that promotes ONLY `CP_TesterPresentReqRsp` to `1` does
/// not re-arm/resend -- `expects_response` is deliberately excluded from
/// `ResolvedTesterPresent::same_wire_behavior` -- so a subsequent ECU
/// response must still be delivered normally: nothing about the ORIGINAL
/// send (made under `ReqRsp = 0`) retroactively becomes discard-eligible
/// just because live config later says `ReqRsp = 1`. Before this fix,
/// `build_cll_rx_entries` re-read `l.active.tester_present_req_rsp() == 1`
/// live on every poll tick, so this exact sequence would have wrongly
/// discarded the frame.
#[tokio::test]
#[serial]
async fn reqrsp_0_to_1_flip_does_not_retroactively_discard() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    // CP_TesterPresentReqRsp explicitly left at its default (0) for the arm.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 0).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    // ADR-137: a plain ISO15765 CLL defaults CP_TesterPresentHandling to 0
    // (ISO_15765_4 = 0), so it must be explicitly promoted to 1 for
    // CoptStartcomm below to arm tester-present at all.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Arm-time send happens under CP_TesterPresentReqRsp = 0 -- with the fix,
    // no discard window should open for it.
    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    let sent_count = server.backdoor.written_count(MOCK_CHANNEL_ID);
    assert_eq!(
        sent_count, 1,
        "the arm-time send should have gone out exactly once"
    );

    // Flip CP_TesterPresentReqRsp to 1. `expects_response` is excluded from
    // `same_wire_behavior`, so this must NOT re-arm/resend.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        sent_count,
        "a CP_TesterPresentReqRsp-only CoptUpdateparam must not re-arm/resend tester-present"
    );
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the frame below
    // delivered under the new unbound-discard model.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "a frame matching ExpPosResp's prefix must be delivered, not discarded: the send that \
         opened (or here, did not open) the discard window happened under \
         CP_TesterPresentReqRsp = 0, and a later live flip to 1 (via a CoptUpdateparam that \
         does not re-arm) must not retroactively make it discard-eligible"
    );
    assert_eq!(delivered.expect("captured").data_bytes, vec![0x7E, 0x00]);

    drop(events);
    server.shutdown().await;
}

/// ADR-088 second amendment: the mirror image of
/// `reqrsp_0_to_1_flip_does_not_retroactively_discard`. A CLL armed under
/// `CP_TesterPresentReqRsp = 1` legitimately opens a discard window (frozen
/// at the send instant). A later `CoptUpdateparam` that promotes ONLY
/// `CP_TesterPresentReqRsp` to `0` does not re-arm/resend (same
/// `same_wire_behavior` exclusion as the 0->1 test), so the still-open
/// window's frozen snapshot must continue to govern: the ECU's tester-present
/// acknowledgement, genuinely elicited by the ReqRsp=1 send, must still be
/// discarded even though live config now says ReqRsp=0. Before this fix,
/// `build_cll_rx_entries`'s live re-read of `CP_TesterPresentReqRsp` would
/// have let this reply leak through to the client the instant the live flip
/// landed.
#[tokio::test]
#[serial]
async fn reqrsp_1_to_0_flip_still_discards_frozen_window() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    // ADR-137: a plain ISO15765 CLL defaults CP_TesterPresentHandling to 0
    // (ISO_15765_4 = 0), so it must be explicitly promoted to 1 for
    // CoptStartcomm below to arm tester-present at all.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Arm-time send happens under CP_TesterPresentReqRsp = 1 -- a discard
    // window legitimately opens.
    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    let sent_count = server.backdoor.written_count(MOCK_CHANNEL_ID);
    assert_eq!(
        sent_count, 1,
        "the arm-time send should have gone out exactly once"
    );

    // Flip CP_TesterPresentReqRsp to 0. This must NOT re-arm/resend (same
    // same_wire_behavior exclusion as the 0->1 direction).
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 0).await;
    promote_via_update_param(&mut client, cll_handle).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        sent_count,
        "a CP_TesterPresentReqRsp-only CoptUpdateparam must not re-arm/resend tester-present"
    );

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "the frame should still be discarded: the still-open window's send-time snapshot \
         (ReqRsp = 1 at the actual send instant) must be honored regardless of the later live \
         flip to ReqRsp = 0"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-088 second amendment: `CP_TesterPresentExpPosResp` is frozen into the
/// `DiscardWindow` at the send instant, the same as `CP_TesterPresentReqRsp`
/// itself -- a later `CoptUpdateparam` that changes ONLY
/// `CP_TesterPresentExpPosResp` does not re-arm/resend (also excluded from
/// `same_wire_behavior`), so the window keeps matching against the ORIGINAL
/// pattern, not the newly-live one. Before this fix, `build_cll_rx_entries`
/// re-read `l.active.tester_present_exp_pos_resp()` live on every poll tick,
/// so this exact sequence would have used the new pattern instead.
#[tokio::test]
#[serial]
async fn exp_pos_resp_flip_uses_frozen_pattern() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    // ADR-137: a plain ISO15765 CLL defaults CP_TesterPresentHandling to 0
    // (ISO_15765_4 = 0), so it must be explicitly promoted to 1 for
    // CoptStartcomm below to arm tester-present at all.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    let sent_count = server.backdoor.written_count(MOCK_CHANNEL_ID);
    assert_eq!(
        sent_count, 1,
        "the arm-time send should have gone out exactly once"
    );

    // Flip CP_TesterPresentExpPosResp only. This must NOT re-arm/resend
    // (same_wire_behavior does not compare ExpPosResp).
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x6E],
    )
    .await;
    promote_via_update_param(&mut client, cll_handle).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        sent_count,
        "a CP_TesterPresentExpPosResp-only CoptUpdateparam must not re-arm/resend tester-present"
    );
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the 0x6E-payload
    // frame below delivered under the new unbound-discard model -- tester-
    // present's own discard (rank 3) still outranks it for the frozen-
    // pattern frame.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // A frame matching the FROZEN pattern (0x7E, from arm time) is still
    // discarded, even though live config now says 0x6E.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7E, 0x00]),
        j2534_0404::ISO15765,
    );
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a frame matching the FROZEN ExpPosResp pattern (0x7E, from arm time) should still be \
         discarded, regardless of the later live flip to 0x6E"
    );

    // A frame matching the NEW live pattern (0x6E) does NOT match the frozen
    // window and is delivered normally.
    let payload = vec![0x6E, 0x00];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::ISO15765,
    );
    let mut delivered: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                delivered = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "a frame matching only the new live ExpPosResp pattern (0x6E), not the frozen one \
         (0x7E), must be delivered, not discarded"
    );
    assert_eq!(delivered.expect("captured").data_bytes, payload);

    drop(events);
    server.shutdown().await;
}

/// ADR-099: `CP_TesterPresentReqRsp = 1`'s discard now also covers a
/// START_OF_MESSAGE herald of the ECU's own tester-present response (empty
/// payload, `rx_status = 0x00000002`), not just the reassembled response
/// content itself -- an indication-type frame like this would otherwise leak
/// through to the client as an unsolicited `ResultData` before the actual
/// (content-matching) response ever arrived. Mirrors
/// `reqrsp_1_discards_positive_response_prefix_match`'s assertion style.
#[tokio::test]
#[serial]
async fn reqrsp_1_discards_som_herald_of_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    // ADR-137: a plain ISO15765 CLL defaults CP_TesterPresentHandling to 0
    // (ISO_15765_4 = 0), so it must be explicitly promoted to 1 for
    // CoptStartcomm below to arm tester-present at all.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // START_OF_MESSAGE herald on the response CAN ID, empty payload, inside
    // the still-open CP_P2Max window.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::ISO15765,
        0x0000_0002, // START_OF_MESSAGE
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a START_OF_MESSAGE herald of the tester-present response should have been discarded, \
         not delivered as ResultData (ADR-099)"
    );

    drop(events);
    server.shutdown().await;
}

/// Creates an ISO15765 CLL, connects it, and configures functional
/// (broadcast) addressing (`CP_RequestAddrMode = 2`, `CP_CanFuncReqId =
/// 0x7DF`) plus a non-empty `CP_TesterPresentMessage`/non-zero
/// `CP_TesterPresentTime`/generous `CP_P2Max` -- mirrors
/// `reqrsp_1_functional_addressing_discards_any_routed_ecu`'s setup, but
/// deliberately leaves the `UniqueRespIdTable` EMPTY.
///
/// This is the only way to actually exercise `tx_can_id`-gated TX-side
/// discard in this harness: `route_frame` only routes a CAN ID that is a
/// UniqueRespIdTable entry's own `CP_CanRespUSDTId`/`CP_CanRespUUDTId` (or,
/// with an empty table, routes every CAN ID unconditionally with
/// `unique_resp_identifier = 0`, ADR-007's no-table wildcard mode) --
/// tester-present's own TX CAN ID (`framed_data`'s leading 4 bytes,
/// `CP_CanFuncReqId` here) is never itself a table entry, so a physically
/// addressed CLL WITH a table (e.g. `create_routed_cll`) would have any
/// frame on its own phys-req CAN ID dropped by routing before it ever
/// reaches the tester-present discard check at all -- unrelated to whether
/// the fix under test works. Leaving the table empty routes every CAN ID
/// unconditionally, letting a frame on `CP_CanFuncReqId` actually reach
/// `poll_rx_inner`'s discard logic.
async fn create_functional_no_table_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
) -> vci_service_interface::ComLogicalLinkHandle {
    let cll_handle = create_and_connect_cll(
        client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_com_param_unum32(client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    // ADR-138: tester-present's own addressing ComParam, independent of
    // CP_RequestAddrMode above.
    set_com_param_unum32(client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_bytes(client, cll_handle, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(client, cll_handle, j2534_0404::P2_MAX, 5_000_000).await;
    cll_handle
}

/// ADR-099, the core new behavior: the discard window now opens whenever a
/// tester-present send succeeds, regardless of `CP_TesterPresentReqRsp` --
/// TX_DONE/TX_INDICATION and CONFIG_LOOPBACK/TX_MSG_TYPE echoes of
/// tester-present's own send are artifacts of the send itself, not of
/// `CP_TesterPresentReqRsp`'s "does the ECU reply" semantics. With
/// `CP_TesterPresentReqRsp` left at its default (0), a frame flagged
/// TX_INDICATION | TX_MSG_TYPE (`rx_status = 0x00000009`) on tester-present's
/// own TX CAN ID (`create_functional_no_table_cll`'s `CP_CanFuncReqId =
/// 0x7DF`) must still be discarded. Also demonstrates the field-doc claim
/// that `tx_can_id`, unlike `target_can_ids`, is NOT loosened to
/// "unrestricted" under functional addressing: functional addressing still
/// has exactly one TX CAN ID of its own.
#[tokio::test]
#[serial]
async fn reqrsp_0_discards_tx_side_echo_of_own_send() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_functional_no_table_cll(&mut client).await;
    // CP_TesterPresentReqRsp explicitly left at its default (0): this test
    // proves TX-side discard does NOT depend on it.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 0).await;
    // ADR-137: a plain ISO15765 CLL defaults CP_TesterPresentHandling to 0
    // (ISO_15765_4 = 0), so it must be explicitly promoted to 1 for
    // CoptStartcomm below to arm tester-present at all.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // TX_INDICATION | TX_MSG_TYPE echo of tester-present's own send, on its
    // own TX CAN ID (0x7DF, from CP_CanFuncReqId), empty payload, inside the
    // still-open window.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7DF, &[]),
        j2534_0404::ISO15765,
        0x0000_0009, // TX_INDICATION | TX_MSG_TYPE
    );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a TX_DONE/loopback echo of tester-present's own send should be discarded even under \
         CP_TesterPresentReqRsp = 0, proving the discard window now opens unconditionally on a \
         successful send (ADR-099)"
    );

    drop(events);
    server.shutdown().await;
}

/// A SOM herald arriving AFTER the `CP_P2Max` discard window has expired is
/// delivered normally, exactly like the pre-existing
/// `reqrsp_1_mode_1_window_expired_does_not_discard` coverage for content
/// discard -- the window bound applies identically across discard cases
/// (ADR-099).
#[tokio::test]
#[serial]
async fn reqrsp_1_som_delivered_after_window_expires() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    // Overrides create_routed_cll's generous 5 s CP_P2Max with a short,
    // deterministic 50 ms window for this test.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 50_000).await;
    // ADR-151: CP_StartMsgIndEnable defaults to disabled -- explicitly
    // enabled here so the delivery this test is actually about (the herald
    // is delivered once the discard window closes) isn't masked by the
    // unrelated ADR-151 gate.
    set_com_param_unum32(&mut client, cll_handle, CP_START_MSG_IND_ENABLE, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // Comfortably past the 50 ms CP_P2Max window.
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::ISO15765,
        0x0000_0002, // START_OF_MESSAGE
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a SOM herald arriving after the discard window has expired should be delivered, not \
         discarded"
    );

    drop(events);
    server.shutdown().await;
}

/// A TX-side echo arriving AFTER the `CP_P2Max` discard window has expired
/// is delivered normally -- the window bound applies to TX-side discard the
/// same as it does to content/SOM discard (ADR-099). Uses
/// `create_functional_no_table_cll` for the same routing reason as
/// `reqrsp_0_discards_tx_side_echo_of_own_send`.
#[tokio::test]
#[serial]
async fn reqrsp_1_tx_side_delivered_after_window_expires() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_functional_no_table_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    // Overrides create_functional_no_table_cll's generous 5 s CP_P2Max with a
    // short, deterministic 50 ms window for this test.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 50_000).await;
    // ADR-151: CP_TransmitIndEnable defaults to disabled -- explicitly
    // enabled here so the delivery this test is actually about (the TX-side
    // echo is delivered once the discard window closes) isn't masked by the
    // unrelated ADR-151 gate.
    set_com_param_unum32(&mut client, cll_handle, CP_TRANSMIT_IND_ENABLE, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // Comfortably past the 50 ms CP_P2Max window.
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7DF, &[]),
        j2534_0404::ISO15765,
        0x0000_0009, // TX_INDICATION | TX_MSG_TYPE
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a TX-side echo arriving after the discard window has expired should be delivered, not \
         discarded"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-099: RX_BREAK (a bus condition) is never treated as a tester-present
/// artifact, even inside an open discard window and even when combined with
/// another indication bit (here, START_OF_MESSAGE) -- it must always still
/// reach the client.
#[tokio::test]
#[serial]
async fn reqrsp_1_rx_break_never_discarded() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    // ADR-151: CP_StartMsgIndEnable is deliberately left at its default
    // (disabled) here -- RX_BREAK always wins `indication_suppressed`'s
    // precedence unconditionally, even combined with START_OF_MESSAGE, so
    // this ComParam never needs setting for the RX_BREAK cases below.
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // RX_BREAK alone.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::ISO15765,
        0x0000_0004, // RX_BREAK
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "an RX_BREAK frame should never be discarded, even inside an open tester-present \
         discard window"
    );

    // RX_BREAK combined with START_OF_MESSAGE.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::ISO15765,
        0x0000_0006, // RX_BREAK | START_OF_MESSAGE
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "an RX_BREAK frame combined with another indication bit (START_OF_MESSAGE) should still \
         never be discarded"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-099: TX-side discard is gated by `tx_can_id`, tester-present's own
/// frozen outgoing CAN ID -- a TX_DONE/loopback-flagged frame on a DIFFERENT,
/// routed CAN ID must still be delivered, not discarded. Uses the two-entry
/// routed CLL (entry 1: `0x7E8`/phys `0x7E0`, entry 2: `0x7E9`/phys `0x7E1`)
/// so the injected CAN ID (`0x7E9`) is present in the routing table (and so
/// actually reaches the discard check) while never being tester-present's own
/// TX CAN ID (`0x7E0`, from the first entry's `CP_CanPhysReqId`, tester-
/// present's physical target per ADR-050).
#[tokio::test]
#[serial]
async fn reqrsp_1_tx_side_discard_does_not_affect_unrelated_can_id() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_two_entry_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    // ADR-151: CP_TransmitIndEnable defaults to disabled -- explicitly
    // enabled here so the delivery this test is actually about (a
    // TX_DONE/loopback-flagged frame on an unrelated CAN ID is delivered,
    // not discarded) isn't masked by the unrelated ADR-151 gate.
    set_com_param_unum32(&mut client, cll_handle, CP_TRANSMIT_IND_ENABLE, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // TX_INDICATION | TX_MSG_TYPE on entry 2's CAN ID (0x7E9) -- routed, but
    // NOT tester-present's own TX CAN ID (0x7E0, from entry 1's
    // CP_CanPhysReqId, the actual physical target).
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E9, &[]),
        j2534_0404::ISO15765,
        0x0000_0009, // TX_INDICATION | TX_MSG_TYPE
    );

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a TX_DONE/loopback-flagged frame on a CAN ID other than tester-present's own TX CAN ID \
         should be delivered, not discarded"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-099, edge-case-hunter coverage gap: a frame with BOTH
/// START_OF_MESSAGE and TX_MSG_TYPE set (`rx_status = 0x00000003`) has
/// `is_tx_side = true`, so it must be routed to arm (c) [TX-side, gated on
/// `tx_can_id`] and NEVER evaluated by arm (b) [SOM herald, gated on
/// `target_can_ids`] even though the SOM bit is present. Injecting this
/// combination on the RESPONSE CAN ID (`0x7E8`, matches `target_can_ids`
/// but not `tx_can_id` -- `create_routed_cll`'s `CP_CanPhysReqId = 0x7E0`)
/// proves arm (b) is correctly skipped: if the `!is_tx_side` guard were
/// missing, this frame would wrongly match arm (b) and be discarded.
#[tokio::test]
#[serial]
async fn reqrsp_1_som_combined_with_tx_msg_type_is_not_treated_as_som_herald() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_routed_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E],
    )
    .await;
    // ADR-151: CP_StartMsgIndEnable is deliberately left at its default
    // (disabled) here -- the SOM | TX_MSG_TYPE frame below is governed by
    // the bare TX_MSG_TYPE bit (a loopback echo, always delivered
    // regardless of this ComParam), not by the SOM bit, so this ComParam
    // never needs setting for this test either; this test is about
    // `bind_frame`'s arm routing, not about the ADR-151 gate.
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // SOM | TX_MSG_TYPE on the RESPONSE CAN ID (0x7E8) -- matches
    // target_can_ids (what arm (b) would gate on) but not tx_can_id (0x7E0,
    // what arm (c) actually gates on for a TX-side frame) -- must NOT be
    // discarded.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[]),
        j2534_0404::ISO15765,
        0x0000_0003, // START_OF_MESSAGE | TX_MSG_TYPE
    );

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a frame with SOM combined with TX_MSG_TYPE must be routed to arm (c) (TX-side, gated \
         on tx_can_id), not arm (b) (SOM herald, gated on target_can_ids) -- on the response \
         CAN ID (not tx_can_id), it must be delivered, not discarded"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-099, edge-case-hunter coverage gap: the existing TX-side discard
/// tests only ever exercise the combined `TX_INDICATION | TX_MSG_TYPE`
/// value (`0x09`). `is_tx_side`'s `|` is a trivially symmetric OR, but each
/// bit's individual leg was never independently exercised -- proves each
/// one alone also triggers arm (c).
#[tokio::test]
#[serial]
async fn reqrsp_1_discards_tx_side_echo_on_either_bit_alone() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_functional_no_table_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    // ADR-137: a plain ISO15765 CLL defaults CP_TesterPresentHandling to 0
    // (ISO_15765_4 = 0), so it must be explicitly promoted to 1 for
    // CoptStartcomm below to arm tester-present at all.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // TX_MSG_TYPE alone.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7DF, &[]),
        j2534_0404::ISO15765,
        0x0000_0001, // TX_MSG_TYPE
    );
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "TX_MSG_TYPE alone should trigger TX-side discard (arm (c))"
    );

    // TX_INDICATION alone.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7DF, &[]),
        j2534_0404::ISO15765,
        0x0000_0008, // TX_INDICATION
    );
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "TX_INDICATION alone should trigger TX-side discard (arm (c))"
    );

    drop(events);
    server.shutdown().await;
}

/// Creates an ISO14230 (K-line, non-CAN) CLL, connects it with
/// `CP_InitializationSettings = 3` to skip the spec-mandated init sequence
/// (ADR-074, mirrors `startcomm_comparam.rs`'s
/// `iso14230_init_settings_none_skips_init_sequence`), and arms tester-present
/// with a non-empty `CP_TesterPresentMessage`/non-zero `CP_TesterPresentTime`
/// plus a generous `CP_P2Max` -- otherwise the same shape as
/// `create_routed_cll`. K-line has no CAN ID / `UniqueRespIdTable` concept at
/// all, so unlike `create_routed_cll` there is no routing to stage: with no
/// CAN-family addressing, `resolve_tester_present`'s `can_functional` is
/// `None`, so the armed `TesterPresentDiscard::tx_can_id` freezes to `None`
/// too -- the exact condition the `tx_can_id.is_some_and(...)` fix (Codex
/// review finding, PR #100) is about.
async fn create_kline_tester_present_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
) -> vci_service_interface::ComLogicalLinkHandle {
    let cll_handle = create_cll(client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(client, cll_handle, CP_INIT_SETTINGS, 3).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");
    set_com_param_bytes(client, cll_handle, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    // See create_routed_cll: a non-zero interval is required for
    // CoptStartcomm to actually arm tester-present.
    set_com_param_unum32(client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(client, cll_handle, j2534_0404::P2_MAX, 5_000_000).await;
    cll_handle
}

/// Codex review finding (PR #100): `TesterPresentDiscard::tx_can_id` is
/// `None` for non-CAN-family protocols (ISO9141/ISO14230/J1850, since
/// `resolved.can_functional` is `None`), and the TX-side discard arm's
/// condition was `discard.tx_can_id.is_none_or(|id| frame_can_id == Some(id))`
/// -- which, for a `None` `tx_can_id`, evaluated to `true`
/// unconditionally, silently discarding EVERY `TX_MSG_TYPE`/`TX_INDICATION`
/// -flagged frame on a non-CAN-family armed CLL, including an indication
/// frame for a totally unrelated, concurrent send on that same CLL. Fixed by
/// changing `is_none_or` to `is_some_and`, so a `None` `tx_can_id` means arm
/// (c) never fires at all.
///
/// Arms tester-present on a K-line (ISO14230) CLL, inside the still-open
/// `CP_P2Max` window, then injects a `TX_INDICATION`-flagged frame
/// (`rx_status = 0x00000008`) that is unrelated to tester-present's own send
/// (e.g. an indication for some other traffic on the same channel) --
/// confirms it is delivered normally as `ResultData`, not wrongly discarded.
#[tokio::test]
#[serial]
async fn reqrsp_1_tx_side_does_not_discard_unrelated_frame_on_non_can_protocol() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_kline_tester_present_cll(&mut client).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    // ADR-151: CP_TransmitIndEnable defaults to disabled -- explicitly
    // enabled here so the delivery this test is actually about (a
    // TX_INDICATION-flagged frame unrelated to tester-present's own send is
    // delivered, not discarded) isn't masked by the unrelated ADR-151 gate.
    set_com_param_unum32(&mut client, cll_handle, CP_TRANSMIT_IND_ENABLE, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;

    // TX_INDICATION-flagged frame, unrelated to tester-present's own send
    // (e.g. an indication for some other concurrent traffic on this CLL).
    // Empty payload, mirroring every other TX-side-flagged injection in this
    // file (an indication carries no reassembled content of its own). Before
    // the fix, discard.tx_can_id.is_none_or(...) evaluated to `true`
    // unconditionally for the K-line CLL's None tx_can_id, wrongly
    // discarding this frame.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[],
        j2534_0404::ISO14230,
        0x0000_0008, // TX_INDICATION
    );

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "a TX_INDICATION-flagged frame unrelated to tester-present's own send should be \
         delivered, not discarded, on a non-CAN-family (K-line) armed CLL whose tx_can_id is \
         None"
    );

    drop(events);
    server.shutdown().await;
}

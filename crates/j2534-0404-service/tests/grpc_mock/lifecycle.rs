//! End-to-end "standard call sequence" for ISO15765 (CAN/ISO-TP) through the
//! gRPC interface, backed by the `j2534-0404-mock` library — the mock-backed,
//! CI-run counterpart of `tests/live_grpc_flow.rs`.

use serial_test::serial;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest,
    ExpectedResponseData, GetEventItemRequest, GetModuleIdsRequest, GetResourceIdsRequest,
    GetStatusRequest, GetTimestampRequest, GetVersionRequest, LockResourceRequest,
    ModuleConnectRequest, ModuleDisconnectRequest, ModuleHandle, ResourceData,
    StartComPrimitiveRequest, SubscribeEventRequest, UnlockResourceRequest,
    create_com_logical_link_request, error_detail_from_status, event_item, get_event_item_request,
    get_status_request, resource_data, subscribe_event_request,
};

use crate::harness::*;

/// End-to-end "standard call sequence" for ISO15765 (CAN/ISO-TP) through the
/// gRPC interface, backed by the `j2534-0404-mock` library. Mirrors the
/// "Typical gRPC Client Flow" documented in `docs/j2534-0404-architecture.md`
/// §11 (the same flow `tests/live_grpc_flow.rs` drives against real
/// hardware) and the full-lifecycle shape of
/// `iso22900-service/tests/grpc_mock.rs::grpc_server_responds_through_mock_library`,
/// adapted to this service's J2534/ISO15765 RPC surface:
///
/// GetModuleIds -> ModuleConnect -> GetVersion -> GetStatus -> GetTimestamp ->
/// GetResourceIds -> CreateComLogicalLink -> SetComParam (baud rate) ->
/// SetUniqueRespIdTable (ECU addressing) -> LockResource -> UnlockResource ->
/// ConnectComLogicalLink -> SubscribeEvent -> StartComPrimitive(CoptStartcomm)
/// -> StartComPrimitive(CoptSendrecv), round-tripped through the mock's RX
/// queue -> StartComPrimitive(CoptStopcomm) -> StartComPrimitive(CoptStopcomm)
/// again (rejected by the comm-not-started guard, verifying the rich error
/// model's `ErrorDetail` per ADR-105) -> DisconnectComLogicalLink ->
/// DestroyComLogicalLink -> ModuleDisconnect.
#[tokio::test]
#[serial]
async fn iso15765_standard_grpc_call_sequence_round_trips_through_mock() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // ── GetModuleIds → ModuleConnect ────────────────────────────────────
    let module_ids = client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect("get_module_ids should succeed")
        .into_inner();
    let module_handle: ModuleHandle = module_ids
        .module_id_list
        .and_then(|list| list.module_data.into_iter().next())
        .and_then(|data| data.module_handle)
        .expect("at least one module should be reported");
    assert_eq!(module_handle.module_handle, MOCK_MODULE_HANDLE);

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("module_connect should succeed");

    // ── GetVersion ───────────────────────────────────────────────────────
    let version = client
        .get_version(GetVersionRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("get_version should succeed")
        .into_inner();
    let version_data = version
        .version_data
        .expect("version data should be present");
    assert_eq!(version_data.hw_name, "1.0.0");
    assert_eq!(version_data.pdu_api_sw_name, "J2534");

    // ── GetStatus(ModuleHandle) → GetTimestamp ──────────────────────────
    let status = client
        .get_status(GetStatusRequest {
            handle: Some(get_status_request::Handle::ModuleHandle(module_handle)),
        })
        .await
        .expect("get_status should succeed")
        .into_inner();
    assert!(status.status.is_some());

    let timestamp = client
        .get_timestamp(GetTimestampRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("get_timestamp should succeed")
        .into_inner();
    assert!(timestamp.timestamp > 0);

    // ── GetResourceIds("ISO15765") ───────────────────────────────────────
    let resource_ids = client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(module_handle),
            resource_data: Some(ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(resource_data::Protocol::ProtocolName(
                    "ISO15765".to_string(),
                )),
            }),
        })
        .await
        .expect("get_resource_ids should succeed")
        .into_inner();
    let resource_id = resource_ids
        .resource_id_list
        .and_then(|list| list.resource_id_data_array.into_iter().next())
        .and_then(|data| data.resource_id_array.into_iter().next())
        .expect("a resource id should resolve for protocol name ISO15765");
    // "ISO15765" is a legacy alias resolving to `ChannelProtocol::ISO15765`,
    // which now matches the `resources` table's `ISO_15765_2` row --
    // resource ID 0x0206 in the opaque 0x0200 namespace, not the raw J2534
    // protocol ID `j2534_0404::ISO15765` (0x06) returned before the table.
    assert_eq!(resource_id, 0x0206);

    // ── Legacy path: a raw J2534 protocol ID (0x06, outside the 0x0200
    // resources-table namespace) must still resolve via
    // `ChannelProtocol::from_raw` and create a link successfully ──────────
    let legacy_cll_response = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(module_handle),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(j2534_0404::ISO15765),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should still succeed for a legacy raw protocol id")
        .into_inner();
    let legacy_cll_handle = legacy_cll_response
        .cll_handle
        .expect("cll_handle should be present for the legacy resource id");
    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(legacy_cll_handle),
        })
        .await
        .expect("destroy_com_logical_link should succeed for the legacy resource id CLL");

    // ── CreateComLogicalLink → SetComParam(DATA_RATE) ───────────────────
    let cll_response = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(module_handle),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(resource_id),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner();
    let cll_handle: ComLogicalLinkHandle = cll_response
        .cll_handle
        .expect("cll_handle should be present");

    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    // CP_P2Max (µs) is the CoptSendrecv response window (ADR-053); widen it
    // to 2 s so the injected ECU reply below can never miss the window.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 2_000_000).await;

    // ── SetUniqueRespIdTable(ECU addressing): physical UDS request/response
    // pair (0x7E0 tester -> 0x7E8 ECU), before Connect so it takes effect
    // immediately at connect time with no pass-all fallback (ADR-048) ──────
    let phys_req_id = 0x7E0_u32;
    let resp_id = 0x7E8_u32;
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, phys_req_id),
                unum32_param(CP_CAN_RESP_USDT_ID, resp_id),
            ],
        )],
    )
    .await;

    // ── LockResource → UnlockResource ───────────────────────────────────
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_handle),
            lock_mask: 0x01,
        })
        .await
        .expect("lock_resource should succeed");
    client
        .unlock_resource(UnlockResourceRequest {
            cll_handle: Some(cll_handle),
            lock_mask: 0x01,
        })
        .await
        .expect("unlock_resource should succeed");

    // ── ConnectComLogicalLink ────────────────────────────────────────────
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");
    assert_eq!(server.backdoor.baud_rate(MOCK_CHANNEL_ID), 500_000);

    // ── SubscribeEvent ───────────────────────────────────────────────────
    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // ── CoptStartcomm ────────────────────────────────────────────────────
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
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish"
    );

    // ── CoptSendrecv: send a UDS ReadDataByIdentifier(VIN) request and
    // round-trip a simulated ECU response through the mock's RX queue ──────
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
                    acceptance_id: 1,
                    // ADR-051: mask/pattern target ResultData.data_bytes,
                    // which is payload-only for ISO15765 (the CAN ID is
                    // split into extra_info instead) — byte 0 is the first
                    // payload byte, expected to be 0x62, the positive
                    // response SID for ReadDataByIdentifier (0x22).
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x62],
                    unique_resp_ids: vec![1],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");

    // Wait for the request to reach the mock before injecting the response:
    // the service only starts waiting for a matching RX frame after the
    // PassThruWriteMsgs for this request has completed (see
    // `events::handle_send_recv`), so injecting any earlier risks the frame
    // being silently drained by an unrelated poll pass with no probe active.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    let mut expected_request = phys_req_id.to_be_bytes().to_vec();
    expected_request.extend_from_slice(&request_payload);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_request
    );

    let response_payload = vec![0x62, 0xF1, 0x90, b'A', b'B', b'C'];
    let mut response_frame = resp_id.to_be_bytes().to_vec();
    response_frame.extend_from_slice(&response_payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &response_frame, j2534_0404::ISO15765);

    let mut received: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data
                && result.acceptance_id == 1
            {
                received = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "expected a ResultData event for the injected ECU response"
    );
    let result = received.expect("ResultData should have been captured");
    assert_eq!(result.unique_resp_identifier, 1);
    // ADR-051: data_bytes is payload-only for ISO15765; the CAN ID header the
    // mock delivered as the frame's leading 4 bytes is split into extra_info
    // instead. The PASSTHRU_MSG the mock received/echoed still carried the
    // header (asserted above via written_data / response_frame).
    assert_eq!(result.data_bytes, response_payload);
    let extra_info = result
        .extra_info
        .expect("extra_info should carry the CAN ID header");
    assert_eq!(extra_info.header_bytes, resp_id.to_be_bytes().to_vec());
    assert!(extra_info.footer_bytes.is_empty());

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptSendrecv should finish after the matching response"
    );

    // ── CoptStopcomm ─────────────────────────────────────────────────────
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStopcomm should finish"
    );
    drop(events);

    // ── Rich error model: a failing RPC carries an ErrorDetail (ADR-105) ─
    // comm is no longer started after the CoptStopcomm above, so a second
    // CoptStopcomm is rejected by the comm-not-started state guard. This
    // replaces the old GetLastError follow-up call: the client now gets
    // everything GetLastError would have provided in this same failing
    // response, with no separate RPC needed.
    let stopcomm_again_status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err("a second CoptStopcomm after comm already stopped should fail");
    assert_eq!(
        stopcomm_again_status.code(),
        tonic::Code::FailedPrecondition
    );
    let error_detail = error_detail_from_status(&stopcomm_again_status)
        .expect("failing RPC should carry an ErrorDetail");
    assert_eq!(
        error_detail.pdu_error,
        vci_service_interface::PduError::PduErrCllNotStarted as i32
    );

    // ── Teardown: DisconnectComLogicalLink → DestroyComLogicalLink →
    // ModuleDisconnect ───────────────────────────────────────────────────
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");
    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("destroy_com_logical_link should succeed");
    client
        .module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("module_disconnect should succeed");

    server.shutdown().await;
}

/// ADR-105 P2 follow-up: a CLL status transition (`send_cll_status`) must
/// reach a `GetEventItem` poller even when no `SubscribeEvent` subscriber is
/// ever attached -- the same "subscription is not the only delivery path"
/// guarantee `send_error_event` already provides for async error events
/// (`CllQueueItem::Error`), now extended to `CllQueueItem::Status`. Drives
/// `ConnectComLogicalLink` (which emits `PDU_CLLST_ONLINE` via
/// `send_cll_status`, `rpc_link.rs`) with no live subscription open at all,
/// then polls `GetEventItem` and asserts the `CllStatus` event comes back --
/// the counterpart, for status events, to `wait_for_result_data`'s existing
/// no-subscriber `ResultData` polling coverage.
#[tokio::test]
#[serial]
async fn cll_status_reaches_get_event_item_poller_without_a_subscriber() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let module_ids = client
        .get_module_ids(GetModuleIdsRequest {})
        .await
        .expect("get_module_ids should succeed")
        .into_inner();
    let module_handle: ModuleHandle = module_ids
        .module_id_list
        .and_then(|list| list.module_data.into_iter().next())
        .and_then(|data| data.module_handle)
        .expect("at least one module should be reported");

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("module_connect should succeed");

    let cll_response = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(module_handle),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(j2534_0404::ISO15765),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner();
    let cll_handle: ComLogicalLinkHandle = cll_response
        .cll_handle
        .expect("cll_handle should be present");

    // ── ConnectComLogicalLink, deliberately with NO SubscribeEvent call
    // beforehand: `send_cll_status(PDU_CLLST_ONLINE)` (rpc_link.rs) has no
    // live subscriber to deliver to, so this only proves anything if the
    // event also lands in the CLL's `rx_buf` for GetEventItem to find ──────
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut cll_status = None;
    for _ in 0..200 {
        let response = client
            .get_event_item(GetEventItemRequest {
                handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("get_event_item should succeed")
            .into_inner();
        if let Some(item) = response.event_item
            && let Some(event_item::Data::CllStatus(status)) = item.data
        {
            cll_status = Some(status);
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        cll_status,
        Some(vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32),
        "PDU_CLLST_ONLINE should be enqueued for GetEventItem even with no live subscriber"
    );

    // ── Teardown ─────────────────────────────────────────────────────────
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");
    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("destroy_com_logical_link should succeed");
    client
        .module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("module_disconnect should succeed");

    server.shutdown().await;
}

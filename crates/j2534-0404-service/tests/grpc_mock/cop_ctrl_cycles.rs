//! PDU_COP_CTRL_DATA cycle control on `CoptSendrecv` (ADR-053): `Time` as
//! the cyclic-send cycle time, `NumSendCycles` (with `-1` = infinite), and
//! `NumReceiveCycles` (`0` no response required, `n` exact matches, `-1`
//! IS-CYCLIC, `-2` IS-MULTIPLE — ADR-058), with the response window taken
//! from the Active `CP_P2Max` instead of `Time`.

use serial_test::serial;
use vci_service_interface::{
    CancelComPrimitiveRequest, ComLogicalLinkHandle, ComPrimitiveCtrlData, ComPrimitiveHandle,
    ConnectComLogicalLinkRequest, DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest,
    EventNotification, ExpectedResponseData, GetObjectIdRequest, GetStatusRequest, IoCtlRequest,
    ObjectType, PduComPrimitiveStatus, StartComPrimitiveRequest, SubscribeEventRequest, event_item,
    event_notification, get_status_request, io_ctl_request, status_response,
    subscribe_event_request, vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// Queues a `CoptDelay` of `time_ms` -- used to hold the FIFO poll-task queue
/// open across subsequent RPC calls so ordering is structural rather than
/// timing-lucky (same pattern `stopcomm_data_tx.rs`'s helper of the same name
/// uses).
async fn queue_delay(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    time_ms: u32,
) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptDelay as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: time_ms,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptDelay) should succeed");
}

/// Starts a `CoptSendrecv` with explicit PDU_COP_CTRL_DATA cycle fields and
/// returns its COP handle (for `CancelComPrimitive`).
async fn start_send_recv(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    cop_data: Vec<u8>,
    time: u32,
    num_send_cycles: i32,
    num_receive_cycles: i32,
    expected_response_array: Vec<ExpectedResponseData>,
) -> Result<ComPrimitiveHandle, tonic::Status> {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time,
                num_send_cycles,
                num_receive_cycles,
                temp_param_update: 0,
                expected_response_array,
                tx_flag: None,
            }),
        })
        .await
        .map(|response| {
            response
                .into_inner()
                .cop_handle
                .expect("cop_handle should be present")
        })
}

/// An `ExpectedResponseData` accepting any positive ReadDataByIdentifier
/// response (first payload byte 0x62) with the given `acceptance_id`.
fn expect_positive_response(acceptance_id: u32) -> ExpectedResponseData {
    ExpectedResponseData {
        response_type: 0,
        acceptance_id,
        mask_data: vec![0xFF],
        pattern_data: vec![0x62],
        unique_resp_ids: vec![],
    }
}

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

fn is_finished(item: &vci_service_interface::EventItem) -> bool {
    matches!(
        item.data,
        Some(event_item::Data::CopStatus(status))
            if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
    )
}

fn is_cancelled(item: &vci_service_interface::EventItem) -> bool {
    matches!(
        item.data,
        Some(event_item::Data::CopStatus(status))
            if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
    )
}

/// `GetStatus(cop_handle)`, unwrapped to the bare `PduComPrimitiveStatus`
/// (ADR-100 Decision §2's `executing_cop`/detached-tier-2 resolution --
/// `rpc_get_status`'s `CopHandle` branch).
async fn cop_status(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cop_handle: ComPrimitiveHandle,
) -> PduComPrimitiveStatus {
    let response = client
        .get_status(GetStatusRequest {
            handle: Some(get_status_request::Handle::CopHandle(cop_handle)),
        })
        .await
        .expect("get_status(COP) should succeed")
        .into_inner();
    match response.status {
        Some(status_response::Status::CopStatus(s)) => PduComPrimitiveStatus::try_from(s)
            .unwrap_or_else(|_| panic!("unexpected PduComPrimitiveStatus value {s}")),
        other => panic!("get_status(COP) returned unexpected status: {other:?}"),
    }
}

/// Polls `GetStatus(cop_handle)` until it reports `expected`, or panics after
/// ~2 s (same 10 ms/200-iteration budget as `harness::wait_for_written_count`).
async fn wait_for_cop_status(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cop_handle: ComPrimitiveHandle,
    expected: PduComPrimitiveStatus,
) {
    for _ in 0..200 {
        let status = cop_status(client, cop_handle).await;
        if status == expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "expected cop_handle {cop_handle:?} to reach {expected:?} within ~2s, last seen: {:?}",
        cop_status(client, cop_handle).await
    );
}

/// Polls `GetEventItem(cll)` (never `SubscribeEvent`, per ADR-115's
/// single-consumer rule: a live subscriber IS the queue's drain, so this is
/// only valid with no subscription open on `cll_handle`), draining and
/// accumulating every item it yields (each call pops one item FIFO) until
/// the accumulated list contains a `CopStatus` item for `cop_handle` equal to
/// `until_status`, or panics after ~2 s. Used to assert the *queued* ordering
/// of a COP's status transitions (`send_cop_status`'s `deliver_or_enqueue`
/// path) the same way `wait_for_event` on a live `SubscribeEvent` stream
/// asserts the live-delivery ordering elsewhere in this file.
async fn drain_cop_status_items_until(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    cop_handle: u32,
    until_status: PduComPrimitiveStatus,
) -> Vec<PduComPrimitiveStatus> {
    let mut statuses = Vec::new();
    for _ in 0..200 {
        let response = client
            .get_event_item(vci_service_interface::GetEventItemRequest {
                handle: Some(
                    vci_service_interface::get_event_item_request::Handle::CllHandle(cll_handle),
                ),
            })
            .await
            .expect("get_event_item should succeed")
            .into_inner();
        if let Some(item) = response.event_item {
            if let Some(event_item::Data::CopStatus(status)) = item.data
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop_handle)
            {
                let status = PduComPrimitiveStatus::try_from(status)
                    .unwrap_or_else(|_| panic!("unexpected PduComPrimitiveStatus value {status}"));
                statuses.push(status);
                if status == until_status {
                    return statuses;
                }
            }
            continue; // more may already be queued; poll again immediately
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "expected cop_handle {cop_handle} to reach {until_status:?} via GetEventItem within \
         ~2s, statuses seen so far: {statuses:?}"
    );
}

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)` -- mirrors `pdu_ioctl.rs`'s helper of the
/// same name.
async fn resolve_ioctl_id(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    name: &str,
) -> u32 {
    client
        .get_object_id(GetObjectIdRequest {
            object_type: ObjectType::ObjtIoCtrl as i32,
            shortname: name.to_string(),
        })
        .await
        .unwrap_or_else(|err| panic!("get_object_id({name}) should succeed: {err}"))
        .into_inner()
        .pdu_object_id
}

/// Runs `IoCtl` against a CLL handle with no input/output payload -- mirrors
/// `pdu_ioctl.rs`'s helper of the same name.
async fn io_ctl_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    cmd_id: u32,
) -> Result<(), tonic::Status> {
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data: None,
            has_output: false,
        })
        .await
        .map(|_| ())
}

/// `NumSendCycles < -1` and `NumReceiveCycles < -2` have no meaning in
/// PDU_COP_CTRL_DATA and are rejected up front.
#[tokio::test]
#[serial]
async fn sendrecv_rejects_out_of_range_cycle_counts() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let status = start_send_recv(&mut client, cll_handle, vec![0x01], 0, -2, 0, vec![])
        .await
        .expect_err("num_send_cycles below -1 should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("num_send_cycles"),
        "{}",
        status.message()
    );

    let status = start_send_recv(&mut client, cll_handle, vec![0x01], 0, 1, -3, vec![])
        .await
        .expect_err("num_receive_cycles below -2 should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("num_receive_cycles"),
        "{}",
        status.message()
    );

    server.shutdown().await;
}

/// `NumSendCycles = 3` with `Time = 0`: the request is written three times
/// (each follow-up cycle re-enqueued at the back of the TX queue), and
/// exactly one `PduCopstFinished` ends the COP after the last cycle.
#[tokio::test]
#[serial]
async fn sendrecv_num_send_cycles_sends_that_many_times() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(&mut client, cll_handle, vec![0x01, 0x02], 0, 3, 0, vec![])
        .await
        .expect("start_com_primitive should succeed");

    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "the COP should finish after the last send cycle"
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 3);
    for index in 0..3 {
        assert_eq!(
            server.backdoor.written_data(MOCK_CHANNEL_ID, index),
            can_frame(0x7E0, &[0x01, 0x02]),
        );
    }

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// `Time > 0`: follow-up cycles are scheduled `Time` ms after the previous
/// cycle started, so a 2-cycle COP with a 300 ms cycle time cannot finish
/// before ~300 ms — and both sends reach the adapter.
#[tokio::test]
#[serial]
async fn sendrecv_cycle_time_schedules_follow_up_cycles() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let started = std::time::Instant::now();
    let cop = start_send_recv(&mut client, cll_handle, vec![0x3E, 0x00], 300, 2, 0, vec![])
        .await
        .expect("start_com_primitive should succeed");

    assert!(
        wait_for_event(&mut events, 3000, |item| is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop.cop_handle))
        .await,
        "the COP should finish after the second cycle"
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(280),
        "the second cycle should not run before the 300 ms cycle time elapsed (took {elapsed:?})"
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 2);

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// `NumSendCycles = -1`: infinite cyclic send; the COP keeps writing on
/// every cycle until `CancelComPrimitive` ends it with `PduCopstCancelled`,
/// after which no further cycles run.
#[tokio::test]
#[serial]
async fn sendrecv_infinite_send_cycles_run_until_cancelled() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x3E, 0x00],
        100,
        -1,
        0,
        vec![],
    )
    .await
    .expect("start_com_primitive should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "the infinite cyclic COP should end as cancelled"
    );

    let count_after_cancel = server.backdoor.written_count(MOCK_CHANNEL_ID);
    tokio::time::sleep(tokio::time::Duration::from_millis(350)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        count_after_cancel,
        "no further cycles may run after cancellation"
    );

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// `NumReceiveCycles = 2`: the receive phase needs two matching responses;
/// one match keeps the COP executing (the window restarts), the second one
/// finishes it.
#[tokio::test]
#[serial]
async fn sendrecv_num_receive_cycles_requires_that_many_matches() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 3_000_000),
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
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        2,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // First matching response: delivered with the descriptor's
    // acceptance_id, but the COP must not finish yet.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    let mut first: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                first = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "the first matching response should be delivered"
    );
    assert_eq!(first.expect("captured").acceptance_id, 9);
    assert!(
        !wait_for_event(&mut events, 300, is_finished).await,
        "one of two required matches must not finish the COP"
    );

    // Second matching response completes the receive phase.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x02]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "the second match should finish the COP"
    );

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// `NumReceiveCycles = -2` (IS-MULTIPLE): every matching response within the
/// `CP_P2Max` window is collected (multiple ECUs answering one request), the
/// window closing is the normal end of the COP, and no `PduErrEvtRxTimeout`
/// is emitted when at least one response arrived.
#[tokio::test]
#[serial]
async fn sendrecv_is_multiple_collects_all_matches_within_the_window() {
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -2,
        vec![expect_positive_response(5)],
    )
    .await
    .expect("start_com_primitive should succeed");
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
    // finishes — with no RxTimeout error along the way.
    let mut accepted = 0;
    let mut saw_rx_timeout = false;
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
                _ => {}
            }
            is_finished(item)
                && item
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

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// Codex review, PR #92 (ADR-087 amendment): `CoptStopcomm`'s otherwise
/// identical IS-MULTIPLE receive phase now clamps its per-match deadline
/// reset to an absolute `match_reset_ceiling_ms` (anchored once at
/// receive-phase entry, `max(16 x CP_P2Max, 2000ms)` --
/// `j2534-0404-service/src/service/events.rs`,
/// `stopcomm_data_tx.rs::stopcomm_is_multiple_chatty_ecu_bounded_by_match_reset_ceiling`).
/// `CoptSendrecv`'s own IS-MULTIPLE receive phase must stay completely
/// unaffected: it carries no such ceiling (`match_reset_ceiling_ms: None`),
/// staying unbounded because it is cancellable via `CancelComPrimitive`,
/// unlike CoptStopcomm's non-cancellable phase. With the same 50 ms
/// `CP_P2Max` the StopComm regression test uses (so the two are directly
/// comparable), a chatty ECU answering every 20 ms must still leave this COP
/// waiting well past the 2 s mark that would end an equivalent StopComm.
#[tokio::test]
#[serial]
async fn sendrecv_is_multiple_not_bounded_by_stopcomm_match_reset_ceiling() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 50_000), // 50 ms window
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -2,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Chatter a matching response every 20 ms while polling events, for
    // 2.3 s -- past the ~2 s floor that bounds CoptStopcomm's otherwise
    // identical IS-MULTIPLE receive phase. CoptSendrecv's IS-MULTIPLE
    // carries no ceiling, so the COP must still be waiting once this loop
    // ends.
    let start = tokio::time::Instant::now();
    let run_until = start + tokio::time::Duration::from_millis(2_300);
    let mut finished = false;
    while tokio::time::Instant::now() < run_until {
        server.backdoor.inject_rx(
            MOCK_CHANNEL_ID,
            &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
            j2534_0404::ISO15765,
        );
        let remaining = run_until.saturating_duration_since(tokio::time::Instant::now());
        let step = tokio::time::Duration::from_millis(20).min(remaining);
        match tokio::time::timeout(step, events.message()).await {
            Ok(Ok(Some(notification))) => {
                if let Some(event_notification::EventData::Item(item)) = notification.event_data
                    && is_finished(&item)
                    && item
                        .cop_handle
                        .as_ref()
                        .is_some_and(|h| h.cop_handle == cop_handle.cop_handle)
                {
                    finished = true;
                    break;
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) => break, // stream ended or errored
            Err(_) => {}                        // this step's poll timed out; loop and inject again
        }
    }

    assert!(
        !finished,
        "CoptSendrecv's IS-MULTIPLE receive phase must not be bounded by \
         CoptStopcomm's match_reset_ceiling_ms -- it should still be waiting \
         after 2.3s of continuous matching responses"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "the still-waiting COP should end as cancelled"
    );

    // Close the event stream before shutting down: graceful shutdown waits
    // for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// `NumReceiveCycles = -1` (IS-CYCLIC): the receive phase has no response
/// window — responses arriving long after `CP_P2Max` are still accepted and
/// the COP keeps receiving until `CancelComPrimitive`.
#[tokio::test]
#[serial]
async fn sendrecv_is_cyclic_receives_until_cancelled() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Deliberately leave CP_P2Max at its 50 ms default: IS-CYCLIC must keep
    // receiving far beyond it.
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Well past the 50 ms default window, a response is still accepted.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    let mut late: Option<vci_service_interface::ResultData> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                late = Some(result.clone());
                return true;
            }
            false
        })
        .await,
        "an IS-CYCLIC receive phase should accept responses beyond CP_P2Max"
    );
    assert_eq!(late.expect("captured").acceptance_id, 3);
    assert!(
        !wait_for_event(&mut events, 300, is_finished).await,
        "IS-CYCLIC must not finish on its own"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "the IS-CYCLIC COP should end as cancelled"
    );

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// ADR-100 Decision §2 tier migration (S5): once an IS-CYCLIC COP's receive
/// phase accepts its first positive response, it migrates to tier 2 and
/// frees the poll task -- a sibling CLL sharing the same physical channel
/// can now execute its own `CoptSendrecv` while the cyclic COP keeps
/// receiving, closing the wedge ADR-100's Context section documents (pre-fix,
/// no other ComPrimitive -- same CLL or sibling -- could run until the
/// IS-CYCLIC COP was cancelled).
#[tokio::test]
#[serial]
async fn sendrecv_is_cyclic_detach_lets_a_sibling_cll_execute() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
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
        cll_b,
        vec![ecu_entry(
            2,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E9),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E1),
            ],
        )],
    )
    .await;

    let mut events_a = subscribe(&mut client, cll_a).await;
    let mut events_b = subscribe(&mut client, cll_b).await;

    let cop_a = start_send_recv(
        &mut client,
        cll_a,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // cll_b's own CoptSendrecv is queued behind cll_a's still-executing
    // IS-CYCLIC COP -- pre-fix, this would never run until cll_a's COP is
    // cancelled (the wedge this ADR fixes).
    start_send_recv(
        &mut client,
        cll_b,
        vec![0x22, 0xF1, 0x91],
        0,
        1,
        1,
        vec![expect_positive_response(7)],
    )
    .await
    .expect("start_com_primitive should succeed");

    // cll_a's first positive response arrives -- migrates the COP to tier 2
    // and frees the poll task for cll_b's queued item.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "cll_a's IS-CYCLIC COP should accept its first response"
    );

    // cll_b's own COP must now be able to execute: its write reaches the
    // adapter, and its own response is matched and delivered, instead of
    // staying wedged behind cll_a's still-alive IS-CYCLIC COP.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E9, &[0x62, 0xF1, 0x91, 0x02]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events_b, 2000, is_finished).await,
        "cll_b's CoptSendrecv should complete normally while cll_a's cyclic COP is still alive"
    );

    // cll_a's COP is still alive (only detached, not finished).
    assert!(
        !wait_for_event(&mut events_a, 200, is_finished).await,
        "the detached IS-CYCLIC COP must not finish on its own"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events_a, 2000, is_cancelled).await,
        "the detached IS-CYCLIC COP should end as cancelled"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// Same-CLL variant of the sibling test above: `StartComPrimitive` enqueues
/// a `CoptSendrecv` without waiting for an earlier COP on the same CLL to
/// finish (no same-CLL serialization guard), so a second COP queued behind
/// a still-executing IS-CYCLIC COP is client-reachable -- confirming the
/// detach also un-wedges same-CLL follow-up COPs, not just sibling CLLs.
#[tokio::test]
#[serial]
async fn sendrecv_is_cyclic_detach_lets_a_same_cll_second_cop_execute() {
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Queued behind cop_a, still executing at this point (nothing dispatches
    // it until cop_a's receive phase returns control to the poll task).
    let cop_b = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x91],
        0,
        1,
        1,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "cop_a should accept its first response and detach"
    );

    // cop_b must now execute -- its write reaches the adapter -- instead of
    // staying wedged behind cop_a.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x91, 0x02]),
        j2534_0404::ISO15765,
    );

    let mut saw_b_result = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data
                && result.acceptance_id == 9
            {
                saw_b_result = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ) && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_b.cop_handle)
        })
        .await,
        "cop_b should execute and finish once cop_a detaches"
    );
    assert!(
        saw_b_result,
        "cop_b's own response should be matched to cop_b, not cop_a"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(wait_for_event(&mut events, 2000, is_cancelled).await);

    drop(events);
    server.shutdown().await;
}

/// A2-1 (ISO 22900-2 §9.4.7 c) / §9.6.2, `iso22900-2-conformance-audit.md`):
/// with two concurrent `CoptSendrecv` COPs alive on one CLL -- `cop_a`
/// detached (IS-CYCLIC, tier-2, still alive) and `cop_b` queued behind it,
/// then executing on its own -- an async error event that pertains to only
/// one of them (here, `cop_b`'s N_Bs-style `CP_P2Max` receive timeout, the
/// exact scenario the finding cites) must carry `cop_b`'s own `cop_handle`,
/// not `cop_a`'s and not `PDU_HANDLE_UNDEF`/`None` -- so a client watching
/// both COPs can tell which one actually failed. Checked through BOTH
/// observation paths this crate supports: the live `SubscribeEvent`
/// notification (`make_cll_notification`) and the queued `GetEventItem`
/// polling drain (`rpc_get_event_item`'s `CllQueueItem::Error` ->
/// `EventItem` construction in `rpc_primitive.rs`) -- two independent
/// construction sites that both read the same `TrackedError.cop`.
#[tokio::test]
#[serial]
async fn sendrecv_error_event_carries_the_failing_cops_own_handle_not_a_concurrent_sibling() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 300_000), // 300ms
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
    let mut events = subscribe(&mut client, cll_handle).await;

    // cop_a: IS-CYCLIC, stays alive (detached to tier-2) after its first
    // match -- never finishes or times out on its own, so any error event
    // wrongly attributed to it would be a bug this test can catch.
    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // cop_b: queued behind cop_a, still executing at this point -- no
    // same-CLL serialization guard blocks StartComPrimitive itself.
    let cop_b = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x91],
        0,
        1,
        1,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "cop_a should accept its first response and detach"
    );

    // cop_b now executes -- its own write reaches the adapter -- but no
    // response is ever injected for it, so its CP_P2Max window (300ms)
    // expires with PduErrEvtRxTimeout.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    let mut error_cop_handle = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ErrorData(error)) = &item.data
                && *error == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32
            {
                error_cop_handle = Some(item.cop_handle);
            }
            is_finished(item)
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop_b.cop_handle)
        })
        .await,
        "cop_b should time out (no response was ever injected for it) and finish"
    );
    assert_eq!(
        error_cop_handle,
        Some(Some(cop_b)),
        "the PduErrEvtRxTimeout event must carry cop_b's own cop_handle -- not cop_a's \
         (still alive, detached) and not PDU_HANDLE_UNDEF/None"
    );

    // ADR-115 single-consumer correction: with a live, healthy
    // `SubscribeEvent` subscriber attached the whole time, the
    // `PduErrEvtRxTimeout` event above was delivered live and is NOT also
    // queued in `rx_buf` for `GetEventItem` -- a live subscriber IS this
    // queue's drain, matching the proto's documented `SubscribeEvent`
    // contract. This also means `GetEventItem`'s own `EventItem.cop_handle`
    // construction for a `CllQueueItem::Error` no longer needs separate
    // coverage here: it and the live path above both go through the same
    // shared `events::cll_queue_item_to_event_item` conversion now
    // (`rpc_primitive.rs`/`events.rs`), so the `error_cop_handle` assertion
    // on the live stream above already exercises it. `GetEventItem` must
    // find nothing left to drain.
    let get_event_item_response = client
        .get_event_item(vci_service_interface::GetEventItemRequest {
            handle: Some(
                vci_service_interface::get_event_item_request::Handle::CllHandle(cll_handle),
            ),
        })
        .await
        .expect("get_event_item should succeed")
        .into_inner();
    assert!(
        get_event_item_response.event_item.is_none(),
        "rx_buf should be empty -- the error event was delivered live to the active \
         subscriber, never buffered: {:?}",
        get_event_item_response.event_item
    );

    // cop_a is still alive (only detached, not finished) -- confirms the
    // error above was never (mis)delivered against it either.
    assert!(
        !wait_for_event(&mut events, 200, is_finished).await,
        "the detached IS-CYCLIC cop_a must not finish on its own"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(wait_for_event(&mut events, 2000, is_cancelled).await);

    drop(events);
    server.shutdown().await;
}

/// A detached (tier-2) IS-CYCLIC registrant keeps matching further responses
/// via `bind_frame`'s tier-2 scan after migration -- not just its own first
/// match -- and `GetStatus(cop)` reports `PduCopstExecuting` for it even
/// though `executing_cop` no longer names it (ADR-100 Decision §2,
/// "Resolved": `executing_cop` stays a single slot meaning "the TxItem the
/// poll task is dispatching right now"; a detached registrant needs its own
/// separate check).
#[tokio::test]
#[serial]
async fn sendrecv_is_cyclic_detached_registrant_keeps_matching_and_reports_executing() {
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Before the first match, the poll task is actively dispatching cop_a --
    // GetStatus already reports Executing via the pre-existing executing_cop
    // mechanism (ADR-021), unaffected by this change.
    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstExecuting
    );

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "cop_a should accept its first response"
    );

    // Now detached: executing_cop no longer names cop_a (the poll task has
    // moved on), but GetStatus must still report Executing, per spec (the
    // ComPrimitive merely switches to receive-only mode -- it is very much
    // still executing).
    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstExecuting,
        "a detached (tier-2) IS-CYCLIC registrant should report Executing, not Waiting"
    );

    // Two more responses, both still attributed to cop_a via tier-2 --
    // migration is not a one-shot "deliver the first match and go quiet"
    // event, the registrant keeps living and matching.
    for byte in [0x02u8, 0x03u8] {
        server.backdoor.inject_rx(
            MOCK_CHANNEL_ID,
            &can_frame(0x7E8, &[0x62, 0xF1, 0x90, byte]),
            j2534_0404::ISO15765,
        );
        let mut matched = None;
        assert!(
            wait_for_event(&mut events, 2000, |item| {
                if let Some(event_item::Data::ResultData(result)) = &item.data {
                    matched = Some(result.clone());
                    return true;
                }
                false
            })
            .await,
            "a further response should still be attributed to the detached cop"
        );
        let result = matched.expect("captured");
        assert_eq!(result.acceptance_id, 3);
        assert_eq!(result.data_bytes, vec![0x62, 0xF1, 0x90, byte]);
    }

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(wait_for_event(&mut events, 2000, is_cancelled).await);

    drop(events);
    server.shutdown().await;
}

/// `CancelComPrimitive` on a detached (tier-2) IS-CYCLIC COP must still emit
/// `PduCopstCancelled` and fully remove its registrant -- nothing is
/// actively "waiting" on the COP any more to notice the cancel marker, so
/// this exercises the new per-tick reap
/// (`events::reap_cancelled_detached_registrants`) rather than the inline
/// receive-phase loop's own per-pass `cancelled_cops` check (which only
/// applies pre-detach).
#[tokio::test]
#[serial]
async fn sendrecv_is_cyclic_cancel_after_detach_removes_registrant() {
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "cop_a should accept its first response and detach"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("cancel_com_primitive on a detached cop should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "the detached IS-CYCLIC COP should end as cancelled via the per-tick reap"
    );

    // GetStatus now reports Cancelled -- the COP is fully gone from
    // `primitives`, not merely no longer Executing, and ADR-128's
    // `terminal_cops` ledger remembers its real terminal status rather than
    // the old code's unconditional Finished-on-miss assumption.
    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstCancelled
    );

    // A second CancelComPrimitive on the same (now-gone-from-`primitives`)
    // handle must now SUCCEED as a no-op (ADR-128/A2-23: ISO 22900-2
    // §9.2.6.6 -- an already-terminal COP takes "no further action" on
    // Cancel, not PDU_ERR_INVALID_HANDLE) as long as this CLL is still
    // alive. This is no longer proof that the registrant/`primitives` entry
    // is actually gone (a still-live mark-and-defer entry in `primitives`
    // also returns success on a second Cancel) -- the frame-attribution
    // check below is what proves that instead.
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("a second cancel of an already-terminal cop should succeed as a no-op");

    // A further matching frame must no longer be attributed to cop_a --
    // its registrant is gone, not just its tier flag.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x02]),
        j2534_0404::ISO15765,
    );
    assert!(
        !wait_for_event(&mut events, 300, |item| item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cop_a.cop_handle))
        .await,
        "no further event should be attributed to cop_a once its registrant is removed"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-095 amendment (2026-07-18, PR #103 Codex review): a detached (tier-2)
/// registrant's cancellation must be reaped promptly even while this
/// channel's single poll task is deep inside a SIBLING COP's own
/// `wait_for_expected_response` receive-phase loop -- not only after that
/// wait finally returns. Before this amendment, `reap_cancelled_detached_
/// registrants`/`reap_expired_cyclic_registrants` were wired only into
/// `run_due_tick_duties`, unreachable for the whole duration of any other
/// long poll-task hold (this file's `handle_send_recv`/`wait_for_expected_
/// response` are driven by the exact same single per-physical-channel task
/// as `run_due_tick_duties` -- see `poll_channel_events`'s single `tx_rx`
/// dequeue loop).
///
/// `cop_a` is an IS-CYCLIC `CoptSendrecv` that detaches to tier-2 after its
/// first accepted match (same setup as `sendrecv_is_cyclic_cancel_after_
/// detach_removes_registrant` above). `cop_b` is then started with a 500ms
/// `CP_P2Max` window and an `ExpectedResponseData` pattern that never
/// matches any injected frame, so its receive-phase loop occupies this
/// channel's poll task for the (nearly) full 500ms, looping through the
/// exact per-pass loop bottom `run_detached_registrant_maintenance` is now
/// injected alongside (`events.rs`'s `dispatch_due_tester_present(true,
/// None, None, false, ctx)` call at the bottom of that loop). `cop_a` is
/// cancelled shortly after `cop_b`'s wait begins; its `PduCopstCancelled`
/// must land well within `cop_b`'s 500ms window, not only after it.
#[tokio::test]
#[serial]
async fn sendrecv_detached_registrant_cancel_reaped_during_a_siblings_receive_phase_wait() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 500_000),
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
    let mut events = subscribe(&mut client, cll_handle).await;

    // cop_a: IS-CYCLIC, detaches to tier-2 on its first accepted match.
    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "cop_a should accept its first response and detach"
    );

    // cop_b: occupies this channel's poll task inside wait_for_expected_
    // response's receive-phase loop for its whole 500ms CP_P2Max window --
    // nothing ever matches acceptance_id 9 below.
    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x91],
        0,
        1,
        1,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    // Cancel the already-detached cop_a while cop_b's receive-phase wait is
    // in progress.
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("cancel_com_primitive on a detached cop should succeed");

    let cancel_issued_at = std::time::Instant::now();
    assert!(
        wait_for_event(&mut events, 200, is_cancelled).await,
        "cop_a's cancellation should be reaped promptly even while cop_b's poll task is inside \
         its own receive-phase wait, not only after that wait's 500ms window closes"
    );
    assert!(
        cancel_issued_at.elapsed() < std::time::Duration::from_millis(400),
        "cop_a's cancellation took {:?}, which is suspiciously close to cop_b's 500ms window -- \
         expected it to be reaped within roughly one poll interval instead",
        cancel_issued_at.elapsed()
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-095 amendment (2026-07-18, PR #103 Codex review) sibling of the test
/// above, covering `wait_for_expected_response`'s OTHER long non-tick-duty
/// hold: the RC21 `request_time_ms` chunked retry sleep (a nested wait
/// inside the same function, distinct from the top-level receive-phase loop
/// bottom covered above). `cop_a` again detaches to tier-2 via an accepted
/// first match; `cop_b` then receives NRC 0x21 (BusyRepeatRequest), putting
/// this channel's poll task into an 800ms `CP_RC21RequestTime` chunked sleep
/// before its own re-request. `cop_a`'s `CancelComPrimitive` is issued while
/// that chunked sleep is in progress and must be reaped well within the
/// 800ms wait.
#[tokio::test]
#[serial]
async fn sendrecv_detached_registrant_cancel_reaped_during_a_siblings_rc21_retry_wait() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    const CP_RC21_COMPLETION_TIMEOUT: u32 = 0x8020;
    const CP_RC21_HANDLING: u32 = 0x8021;
    const CP_RC21_REQUEST_TIME: u32 = 0x8022;
    const CP_RC_BYTE_OFFSET: u32 = 0x8028;

    // CP_RC21RequestTime/CP_RC21CompletionTimeout are D-PDU timing ComParams
    // stored in MICROSECONDS (`RcHandlingConfig::from_params`'s `get_us_as_ms`
    // conversion, same convention as CP_P2Max) -- 800_000/5_000_000 here give
    // an actual 800ms request_time_ms wait and a 5000ms completion ceiling,
    // not 800ms/5000ms directly.
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_RC21_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_RC21_REQUEST_TIME, 800_000),
            (CP_RC21_COMPLETION_TIMEOUT, 5_000_000),
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
    let mut events = subscribe(&mut client, cll_handle).await;

    // cop_a: IS-CYCLIC, detaches to tier-2 on its first accepted match.
    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "cop_a should accept its first response and detach"
    );

    // cop_b: triggers NRC 0x21, putting this channel's poll task into the
    // 800ms CP_RC21RequestTime chunked retry sleep.
    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x91],
        0,
        1,
        1,
        vec![expect_positive_response(5)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x22, 0x21]),
        j2534_0404::ISO15765,
    );

    // Give cop_b's RC21 wait a moment to actually begin (the NRC frame must
    // be consumed by a poll pass first).
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("cancel_com_primitive on a detached cop should succeed");

    let cancel_issued_at = std::time::Instant::now();
    assert!(
        wait_for_event(&mut events, 500, is_cancelled).await,
        "cop_a's cancellation should be reaped promptly even while cop_b's poll task is inside \
         the RC21 request_time_ms chunked retry wait, not only after that wait's 800ms window \
         completes"
    );
    assert!(
        cancel_issued_at.elapsed() < std::time::Duration::from_millis(600),
        "cop_a's cancellation took {:?}, which is suspiciously close to cop_b's 800ms RC21 \
         wait -- expected it to be reaped within roughly one poll interval instead",
        cancel_issued_at.elapsed()
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-095 amendment (2026-07-18, PR #103 Codex review), edge-case-hunter
/// follow-up: the two tests above cover `wait_for_expected_response`'s two
/// non-tick-duty holds; `isotp_send`'s own FC-wait (N_Bs) loop needs the
/// identical proof -- `run_detached_registrant_maintenance(ctx)` is injected
/// there too (`events.rs`, immediately before that loop's own
/// `dispatch_due_tester_present` call). `cop_a` again detaches to tier-2 via
/// an accepted first match on `cll_a`. `cll_b` is a sibling CLL sharing the
/// same physical channel (this crate's `isotp_fc_wait_dispatches_due_tester_
/// present_for_a_sibling_cll` harness technique, `tester_present_send_type.rs`)
/// running under `CanChannelMode::SoftwareIsoTp`; its 20-byte `CoptSendrecv`
/// payload requires software ISO-TP segmentation, and with no FlowControl
/// ever injected, cll_b's poll task is blocked in `isotp_send`'s FC-wait
/// loop for its full default 1000ms N_Bs window. `cop_a`'s
/// `CancelComPrimitive` is issued while that wait is in progress and must be
/// reaped well within the 1000ms window, not only once cll_b's own transfer
/// times out.
#[tokio::test]
#[serial]
async fn isotp_fc_wait_reaps_a_siblings_cancelled_detached_registrant() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    // cll_a: IS-CYCLIC CoptSendrecv detaches to tier-2 after its first
    // accepted match (same setup as the two tests above).
    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
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
    let mut events_a = subscribe(&mut client, cll_a).await;

    let cop_a = start_send_recv(
        &mut client,
        cll_a,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "cop_a should accept its first response and detach"
    );

    // cll_b: shares cll_a's physical channel, physically addressed to a
    // different target (0x7A0) so its ISO-TP wire traffic can't be confused
    // with cll_a's own frames. `CP_Bs` (N_Bs) is left at its default 1000ms
    // so the FC-wait below runs for the full window.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7A0).await;
    let mut events_b = subscribe(&mut client, cll_b).await;

    // A 20-byte payload segments into FirstFrame + ConsecutiveFrames; no
    // FlowControl is ever injected, so cll_b's poll task is blocked in
    // isotp_send's FC-wait (N_Bs) loop for the whole 1000ms window.
    let payload: Vec<u8> = (1..=20).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    // cll_a's own single write (above) plus cll_b's FirstFrame -- reaching 2
    // confirms cll_b's isotp_send has entered the FC-wait loop.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("cancel_com_primitive on a detached cop should succeed");

    let cancel_issued_at = std::time::Instant::now();
    assert!(
        wait_for_event(&mut events_a, 300, is_cancelled).await,
        "cop_a's cancellation should be reaped promptly even while cll_b's isotp_send is \
         blocked in its own FC-wait (N_Bs) loop, not only after that wait's 1000ms window \
         completes"
    );
    assert!(
        cancel_issued_at.elapsed() < std::time::Duration::from_millis(700),
        "cop_a's cancellation took {:?}, which is suspiciously close to cll_b's 1000ms N_Bs \
         wait -- expected it to be reaped within roughly one poll interval instead",
        cancel_issued_at.elapsed()
    );

    // Let cll_b's own transfer run out its N_Bs timeout naturally before
    // shutdown, matching this file's existing cleanup convention.
    assert!(
        wait_for_event(&mut events_b, 2000, is_finished).await,
        "expected cll_b's isotp_send to finish (via N_Bs timeout) after its FC-wait window"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// ADR-095 amendment (2026-07-18, PR #103 Codex review) sibling of the test
/// above, covering `isotp_send`'s OTHER non-tick-duty hold: the
/// STmin/ConsecutiveFrame-block pacing loop (a nested loop distinct from the
/// FC-wait loop covered above, reached once a FlowControl frame is
/// received). `cop_a` again detaches to tier-2 via an accepted first match;
/// `cll_b` then sends a 111-byte payload that segments into a FirstFrame
/// plus 15 ConsecutiveFrames (mirroring `isotp_stmin_loop_interleaves_due_
/// tester_present_among_consecutive_frames`'s harness technique in
/// `tester_present_send_type.rs`), with the injected FlowControl's STmin=
/// 127ms pacing the CF block for a total window of roughly 15 * 127ms =~
/// 1.9s. `cop_a`'s `CancelComPrimitive` is issued right after the first
/// ConsecutiveFrame lands on the wire -- i.e. while cll_b's poll task is
/// still deep inside the pacing loop -- and must be reaped promptly, long
/// before the remaining ~14 CFs finish sending.
#[tokio::test]
#[serial]
async fn isotp_stmin_loop_reaps_a_siblings_cancelled_detached_registrant() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    // cll_a: IS-CYCLIC CoptSendrecv detaches to tier-2 after its first
    // accepted match (same setup as the FC-wait sibling test above).
    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
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
    let mut events_a = subscribe(&mut client, cll_a).await;

    let cop_a = start_send_recv(
        &mut client,
        cll_a,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "cop_a should accept its first response and detach"
    );

    // cll_b: shares cll_a's physical channel, physically addressed to a
    // different target (0x7A0) so its ISO-TP wire traffic can't be confused
    // with cll_a's own frames.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7A0).await;
    let mut events_b = subscribe(&mut client, cll_b).await;

    // 111 bytes: Normal addressing's FirstFrame carries 6, leaving exactly
    // 15 ConsecutiveFrames of 7 bytes each.
    let payload: Vec<u8> = (1..=111).collect();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed");

    // cll_a's own single write (above) plus cll_b's FirstFrame -- reaching 2
    // confirms the FirstFrame has been sent.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    // ECU answers with FlowControl: ContinueToSend, BS=0 (send all 15 CFs
    // back-to-back), STmin=127ms -- ~15 * 127ms =~ 1.9s total CF-sending time.
    let mut fc = 0x7A8_u32.to_be_bytes().to_vec();
    fc.extend_from_slice(&[0x30, 0x00, 0x7F]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &fc, j2534_0404::CAN);

    // Reaching 3 confirms the first ConsecutiveFrame has been written --
    // cll_b's poll task is now inside the STmin pacing loop, with ~14 more
    // CFs (~1.78s) still to go.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("cancel_com_primitive on a detached cop should succeed");

    let cancel_issued_at = std::time::Instant::now();
    assert!(
        wait_for_event(&mut events_a, 300, is_cancelled).await,
        "cop_a's cancellation should be reaped promptly even while cll_b's isotp_send is deep \
         inside its own STmin/ConsecutiveFrame-block pacing loop, not only after that loop's \
         ~1.9s window completes"
    );
    assert!(
        cancel_issued_at.elapsed() < std::time::Duration::from_millis(1000),
        "cop_a's cancellation took {:?}, which is suspiciously close to cll_b's ~1.9s STmin \
         pacing window -- expected it to be reaped within roughly one poll interval instead",
        cancel_issued_at.elapsed()
    );

    // Let cll_b's own transfer finish sending all 15 ConsecutiveFrames
    // before shutdown, matching this file's existing cleanup convention.
    assert!(
        wait_for_event(&mut events_b, 4000, is_finished).await,
        "expected cll_b's isotp_send to finish after all 15 ConsecutiveFrames were sent"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// ADR-086 generation-staleness (Decision §2's "or an ADR-086
/// disconnect/reconnect generation change"): a `DisconnectComLogicalLink`
/// call cancels a detached (tier-2) registrant immediately via the existing
/// `cancel_link_cops` wholesale sweep -- the same mechanism that already
/// clears every OTHER outstanding COP and the whole `registrants` Vec on CLL
/// teardown, unaffected by tier.
#[tokio::test]
#[serial]
async fn sendrecv_is_cyclic_detach_registrant_cleared_by_disconnect() {
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "cop_a should accept its first response and detach"
    );

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "disconnecting the CLL should cancel the detached IS-CYCLIC COP immediately, not merely \
         mark it stale for a later pass to notice"
    );
    // ADR-128: GetStatus reports the real recorded terminal status
    // (Cancelled) via `terminal_cops`, not the old unconditional Finished
    // default for any COP no longer in `primitives`.
    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstCancelled
    );

    drop(events);
    server.shutdown().await;
}

/// `NumReceiveCycles = 0`: the send requires no response at all, so the COP
/// completes immediately after the write -- even with a non-empty
/// `expected_response_array` (ADR-058). `CP_P2Max` is set far longer than
/// this test's assertion window and no RX frame is ever injected, so the
/// pre-ADR-058 "0 => wait for one match" behaviour would still be blocked on
/// the receive phase when the assertion below runs, rather than having
/// already finished.
#[tokio::test]
#[serial]
async fn sendrecv_zero_receive_cycles_completes_without_waiting_for_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 5_000_000),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x3E, 0x00],
        0,
        1,
        0,
        vec![expect_positive_response(1)],
    )
    .await
    .expect("start_com_primitive should succeed");

    assert!(
        wait_for_event(&mut events, 500, is_finished).await,
        "NumReceiveCycles = 0 should finish the COP right after the write, without waiting for a response"
    );

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// An empty `expected_response_array` with `NumReceiveCycles > 0` is not a
/// special case (ADR-058): no descriptor can ever match, so the receive
/// phase runs out its `CP_P2Max` window like any other non-matching cycle,
/// ending in a receive timeout rather than finishing silently.
#[tokio::test]
#[serial]
async fn sendrecv_empty_expected_response_with_nonzero_cycles_times_out() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 200_000),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop = start_send_recv(&mut client, cll_handle, vec![0x3E, 0x00], 0, 1, 1, vec![])
        .await
        .expect("start_com_primitive should succeed");

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
            is_finished(item)
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop.cop_handle)
        })
        .await,
        "the COP should finish once the CP_P2Max window closes with no descriptors to match"
    );
    assert!(
        saw_rx_timeout,
        "an empty expected_response_array with NumReceiveCycles > 0 should time out, not finish silently"
    );

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// `NumSendCycles = 0`: no request is ever transmitted (a receive-only
/// capture), but the receive phase runs exactly as it would for a normal
/// send -- an unsolicited frame matching `expected_response_array` still
/// completes the COP (ADR-059).
#[tokio::test]
#[serial]
async fn sendrecv_zero_send_cycles_skips_transmission_and_only_receives() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 2_000_000),
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
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        1,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");

    // Give the poll task time to run this cycle; NumSendCycles = 0 must
    // never reach PassThruWriteMsgs.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "NumSendCycles = 0 must not transmit anything"
    );

    // The receive phase still runs: an unsolicited matching frame (nothing
    // was ever requested) still completes the COP.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
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
        "the receive-only phase should still deliver a matching frame"
    );
    assert_eq!(received.expect("captured").acceptance_id, 9);
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "the COP should finish once the required match arrives"
    );

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// D-PDU `CP_CyclicRespTimeout` ComParam id (ADR-100 Decision §4). Not
/// exported by the crate, so duplicated here as a literal for the test,
/// mirroring `j1850_autodetect.rs`'s own local copy of the same constant.
const CP_CYCLIC_RESP_TIMEOUT: u32 = 0x8010;

/// ADR-100 Decision §4 (S6): a created-receive-only (`NumSendCycles = 0`)
/// `NumReceiveCycles = -1` COP with a nonzero `CP_CyclicRespTimeout`
/// transitions to `PDU_COPST_FINISHED` -- not `PDU_COPST_CANCELLED` -- once
/// the timeout elapses with no match ever received (ISO 22900-2 §9.2.6.3.4
/// RECEIVE ONLY NOTE 1). This registrant is inserted directly as tier-2 at
/// creation and its owning poll task returns immediately (never entering
/// `wait_for_expected_response_inner`'s receive loop), so this exercises
/// `events::reap_expired_cyclic_registrants`'s periodic sweep -- the sole
/// mechanism that ever fires this timeout -- not an inline per-pass check.
#[tokio::test]
#[serial]
async fn receive_only_cyclic_timeout_finishes_with_no_matches_ever_received() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            // 200 ms (stored as µs, ADR-100 Decision §4's resolved µs
            // convention -- `cyclic_resp_timeout_ms()`).
            (CP_CYCLIC_RESP_TIMEOUT, 200_000),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    let is_finished_for_cop = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop.cop_handle)
    };

    // NumSendCycles = 0 must never transmit, same as the plain ADR-059 case.
    tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "a created-receive-only COP must not transmit anything"
    );

    // Well before the 200 ms deadline, the COP is still alive.
    assert!(
        !wait_for_event(&mut events, 60, is_finished_for_cop).await,
        "the cyclic timeout must not fire before CP_CyclicRespTimeout elapses"
    );

    assert!(
        wait_for_event(&mut events, 2000, is_finished_for_cop).await,
        "CP_CyclicRespTimeout must finish the COP once it elapses with no match"
    );

    drop(events);
    server.shutdown().await;
}

/// `CP_CyclicRespTimeout = 0` (the default) means "disabled" -- a
/// created-receive-only `NumReceiveCycles = -1` COP behaves exactly as it
/// did before ADR-100 S6: it never finishes on its own, ending only via
/// `CancelComPrimitive` (ISO 22900-2 §9.2.6.3.4 RECEIVE ONLY NOTE 1,
/// "Otherwise, the application cancels...").
#[tokio::test]
#[serial]
async fn receive_only_cyclic_disabled_timeout_never_finishes_on_its_own() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // CP_CyclicRespTimeout deliberately left unset (defaults to 0).
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    let is_finished_for_cop = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_handle.cop_handle)
    };

    // A generous wait with no match at all -- with the timeout disabled,
    // this must never finish on its own regardless of how long it waits.
    assert!(
        !wait_for_event(&mut events, 400, is_finished_for_cop).await,
        "a disabled CP_CyclicRespTimeout must never finish the COP on its own"
    );

    // One accepted match, then another generous wait -- still alive.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "the receive-only COP should accept a match"
    );
    assert!(
        !wait_for_event(&mut events, 400, is_finished_for_cop).await,
        "still alive after a match, with the timeout disabled"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "the receive-only COP should end as cancelled"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-100 Decision §4 (S6): "deadline restarted per accepted match" -- a
/// created-receive-only `NumReceiveCycles = -1` COP's `CP_CyclicRespTimeout`
/// deadline is pushed forward by every accepted match, so the COP survives
/// well past its ORIGINAL deadline as long as matches keep arriving inside
/// each window, but still times out once they stop.
#[tokio::test]
#[serial]
async fn receive_only_cyclic_deadline_restarts_on_each_match_then_times_out_when_they_stop() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CYCLIC_RESP_TIMEOUT, 200_000), // 200 ms
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
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");

    // Give the registrant time to be inserted (receive phase begins almost
    // immediately since NumSendCycles = 0 skips the write/P3-gap steps).
    tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;

    // Three matches, ~130 ms apart -- each well inside the 200 ms window, so
    // every one restarts the deadline. By the third injection, well over
    // 200 ms has elapsed since the FIRST match (the original, un-restarted
    // deadline would have fired long before this point).
    for byte in [0x01u8, 0x02u8, 0x03u8] {
        server.backdoor.inject_rx(
            MOCK_CHANNEL_ID,
            &can_frame(0x7E8, &[0x62, 0xF1, 0x90, byte]),
            j2534_0404::ISO15765,
        );
        assert!(
            wait_for_event(&mut events, 2000, |item| matches!(
                &item.data,
                Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
            ))
            .await,
            "each injected frame should be accepted as a match"
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(130)).await;
    }

    assert!(
        !wait_for_event(&mut events, 50, is_finished).await,
        "matches kept arriving inside each window, so the COP must still be alive \
         past the original (un-restarted) deadline"
    );

    // Now stop injecting -- the deadline from the last match fires normally.
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "once matches stop arriving, CP_CyclicRespTimeout should finish the COP"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-100 Decision §4's resolved scope: `CP_CyclicRespTimeout` applies only
/// to a created-receive-only (`NumSendCycles = 0`) `NumReceiveCycles = -1`
/// registrant, NOT to a migrated (S5) IS-CYCLIC COP (`NumSendCycles != 0`,
/// tier-1 -> tier-2 via first positive response) -- that COP's
/// `cyclic_deadline` stays `None` even when the ComParam is configured, so it
/// keeps ADR-053's original "ends only via CancelComPrimitive or a hard
/// error" behavior unchanged.
#[tokio::test]
#[serial]
async fn sendrecv_is_cyclic_migrated_registrant_unaffected_by_cyclic_resp_timeout() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CYCLIC_RESP_TIMEOUT, 150_000), // 150 ms -- deliberately short.
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
    let mut events = subscribe(&mut client, cll_handle).await;

    // NumSendCycles = 1 (not 0): this COP is created WITH a send phase, so
    // it starts tier-1 and migrates to tier-2 only at its first positive
    // response (S5) -- ADR-100 Decision §4's resolved scope excludes it.
    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "the migrated COP should accept its first response"
    );

    // Well past the configured 150 ms CP_CyclicRespTimeout, with no further
    // matches -- a created-receive-only COP in this situation would already
    // have finished; a migrated one must not.
    assert!(
        !wait_for_event(&mut events, 500, is_finished).await,
        "CP_CyclicRespTimeout must not apply to a migrated (S5) IS-CYCLIC COP"
    );
    assert_eq!(
        cop_status(&mut client, cop_handle).await,
        PduComPrimitiveStatus::PduCopstExecuting,
        "the migrated COP should still report Executing, not Finished"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "the migrated COP should still end as cancelled"
    );

    drop(events);
    server.shutdown().await;
}

/// `CancelComPrimitive` still works normally on a created-receive-only
/// `NumReceiveCycles = -1` COP while a `CP_CyclicRespTimeout` is configured,
/// cancelled BEFORE the deadline elapses and before any match arrives. This
/// registrant is inserted directly as tier-2 at creation and its owning poll
/// task returns immediately (never entering `wait_for_expected_response_inner`'s
/// receive loop), so it is already detached at the moment of cancellation --
/// reaped by `events::reap_cancelled_detached_registrants`, same as the
/// after-detach case below.
#[tokio::test]
#[serial]
async fn receive_only_cyclic_cancel_works_before_first_match_with_timeout_configured() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CYCLIC_RESP_TIMEOUT, 5_000_000), // 5 s -- far longer than this test runs.
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");

    tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "cancelling before any match, with a timeout configured, should still cancel normally"
    );

    drop(events);
    server.shutdown().await;
}

/// `CancelComPrimitive` still works normally on a created-receive-only
/// `NumReceiveCycles = -1` COP while a `CP_CyclicRespTimeout` is configured,
/// cancelled AFTER the first match already detached it to tier 2 (reaped by
/// `reap_cancelled_detached_registrants`, unaffected by the new
/// `reap_expired_cyclic_registrants` sweep added alongside it).
#[tokio::test]
#[serial]
async fn receive_only_cyclic_cancel_works_after_detach_with_timeout_configured() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CYCLIC_RESP_TIMEOUT, 5_000_000), // 5 s -- far longer than this test runs.
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "the receive-only COP should accept its first match and detach"
    );
    assert_eq!(
        cop_status(&mut client, cop_handle).await,
        PduComPrimitiveStatus::PduCopstExecuting,
        "a detached receive-only registrant should report Executing"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "cancelling a detached receive-only registrant, with a timeout configured, \
         should still cancel normally via the per-tick reap"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-117 regression: a reaped tier-2 registrant must report `Cancelled`, not
/// `Waiting`, once `reap_cancelled_detached_registrants` has removed it from
/// `primitives` -- the `CopEntry::dispatched` flag lives inside that same
/// `primitives` entry, so its removal discards `dispatched` atomically with
/// no separate cleanup to get wrong. Same migrate-then-cancel shape as
/// `receive_only_cyclic_cancel_works_after_detach_with_timeout_configured`,
/// but additionally polls `GetStatus(cop)` once the `Cancelled` event confirms
/// the reap has fully completed (the COP is gone from `primitives`), pinning
/// that it reads `PduCopstCancelled`, not `PduCopstWaiting`. (Pre-ADR-128 this
/// asserted `PduCopstFinished`, since `GetStatus` used to assume Finished
/// unconditionally for any COP no longer in `primitives`; ADR-128's
/// `terminal_cops` ledger now reports the COP's real recorded terminal
/// status, so a cancelled-and-reaped registrant correctly reads Cancelled.)
#[tokio::test]
#[serial]
async fn receive_only_cyclic_cancel_after_detach_reports_cancelled_not_waiting() {
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "the receive-only COP should accept its first match and detach to tier 2"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "a detached receive-only registrant should be cancelled via the per-tick reap"
    );

    // The Cancelled event above only fires once `reap_cancelled_detached_
    // registrants` has already removed the entry from `primitives`, so the
    // COP is now fully gone from `primitives` -- but ADR-128's `terminal_cops`
    // ledger still remembers it was Cancelled, not Finished, so GetStatus
    // must report Cancelled, never Waiting (and never the old unconditional
    // Finished default either).
    assert_eq!(
        cop_status(&mut client, cop_handle).await,
        PduComPrimitiveStatus::PduCopstCancelled,
        "a fully-reaped detached registrant must report Cancelled, not Waiting"
    );

    drop(events);
    server.shutdown().await;
}

/// `NumSendCycles = 0` and `NumReceiveCycles = 0` together: neither a
/// transmission nor a receive phase happens at all -- the COP still goes
/// through its normal `PduCopstExecuting`/`PduCopstFinished` transitions
/// promptly, with no data transfer of any kind (ADR-059).
#[tokio::test]
#[serial]
async fn sendrecv_zero_send_and_zero_receive_cycles_does_neither() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x3E, 0x00],
        0,
        0,
        0,
        vec![expect_positive_response(1)],
    )
    .await
    .expect("start_com_primitive should succeed");

    assert!(
        wait_for_event(&mut events, 500, is_finished).await,
        "NumSendCycles = 0 and NumReceiveCycles = 0 should still finish the COP promptly"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "neither field being 0 should ever transmit anything"
    );

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// ADR-086 (this round's fix): `TxItem::SendRecv` now carries a
/// `connect_generation`, so the pre-dispatch `should_skip_cancelled_item`
/// check (already protecting `CoptStartcomm`/`CoptStopcomm` since ADR-086's
/// round 5) now also skips a stale queued `CoptSendrecv` before it ever
/// reaches `handle_send_recv`. Before this round, `TxItem::connect_generation()`
/// returned `None` for `SendRecv` (a documented, deliberately out-of-scope
/// residual of round 5), so a `CoptSendrecv` queued behind a slow `CoptDelay`,
/// cancelled by `cancel_link_cops` at disconnect, then made stale by a
/// reconnect of the same `cll_handle` elsewhere, still passed the pre-dispatch
/// check, reached `handle_send_recv`, and transmitted its stale, pre-
/// disconnect payload.
///
/// Same technique as `stopcomm_data_tx.rs`'s
/// `stopcomm_stale_queued_item_does_not_tear_down_a_reconnected_sessions_tester_present`:
/// cll_a's `CoptSendrecv` is queued behind a `CoptDelay` that pins its
/// ORIGINAL physical channel's poll task busy for `DELAY_MS`. Disconnecting
/// cll_a immediately cancels the queued SendRecv (`cancel_link_cops`,
/// `PduCopstCancelled`), but the `CoptDelay` itself keeps that channel's poll
/// task -- and the stale SendRecv still parked behind it in the same FIFO --
/// alive (cll_b, kept connected throughout, keeps the ORIGINAL physical
/// channel from tearing down). cll_a is then reconnected at a DIFFERENT
/// `DATA_RATE`, landing on a brand-new physical channel with its own poll
/// task. Once `DELAY_MS` elapses, the ORIGINAL channel's poll task reaches the
/// stale, already-cancelled SendRecv: it must be skipped before dispatch --
/// no further `CopStatus` event (in particular no `PduCopstExecuting`) for
/// its `cop_handle`, and no `PassThruWriteMsgs` call on the ORIGINAL channel.
#[tokio::test]
#[serial]
async fn sendrecv_stale_queued_item_is_skipped_before_dispatch_after_reconnect() {
    const DELAY_MS: u32 = 300;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;

    // Kept alive (not disconnected) so cll_a's ORIGINAL physical channel --
    // and its poll task, still busy with the CoptDelay below -- survives
    // cll_a's disconnect/reconnect-elsewhere.
    let _cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );

    let mut events = subscribe(&mut client, cll_a).await;

    // Hold cll_a's ORIGINAL channel's poll task busy for DELAY_MS.
    queue_delay(&mut client, cll_a, DELAY_MS).await;

    // Queued behind the CoptDelay above -- not yet dispatched.
    let send_cop_handle = start_send_recv(&mut client, cll_a, vec![0x01, 0x02], 0, 1, 0, vec![])
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed (queued behind CoptDelay)")
        .cop_handle;

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    // cancel_link_cops's own immediate cancellation of the still-queued
    // SendRecv. Checked before any further wait_for_event calls so it is not
    // silently discarded (drained and ignored) by one of them.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == send_cop_handle))
        .await,
        "the queued SendRecv COP should have received PduCopstCancelled from cancel_link_cops"
    );

    // Reconnect cll_a at a DIFFERENT DATA_RATE -- a distinct physical
    // channel/poll task from cll_b's (and from the one still holding the
    // stale SendRecv behind the CoptDelay).
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 250_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("reconnecting cll_a at a different DATA_RATE should succeed");
    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "cll_a's reconnect at a different DATA_RATE should open a brand-new physical channel"
    );

    // Past DELAY_MS with margin: the ORIGINAL channel's poll task has by now
    // finished its CoptDelay and reached the stale, already-cancelled
    // SendRecv -- pre-fix, this is where it would have transmitted stale data
    // and emitted PduCopstExecuting/PduCopstFinished for a cop_handle the
    // client already saw Cancelled.
    tokio::time::sleep(std::time::Duration::from_millis(u64::from(DELAY_MS) + 200)).await;

    // No further CopStatus event of any kind for the stale SendRecv's
    // cop_handle: should_skip_cancelled_item's generation check must have
    // skipped it before handle_send_recv ever ran.
    assert!(
        !wait_for_event(&mut events, 200, |item| item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == send_cop_handle))
        .await,
        "no further CopStatus event should be emitted for the stale SendRecv cop_handle -- \
         handle_send_recv must never run for it"
    );

    // No PassThruWriteMsgs call landed on the ORIGINAL channel for the stale
    // SendRecv's stale, pre-disconnect payload.
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the stale SendRecv must never transmit on the ORIGINAL physical channel"
    );

    // Close the event stream before shutting down: graceful shutdown
    // waits for in-flight requests, and an open stream never ends.
    drop(events);

    server.shutdown().await;
}

/// ADR-086 (round 11): `handle_delay`'s own per-tick/terminal
/// `still_on_this_channel` staleness detection, for an ALREADY-DISPATCHED
/// `CoptDelay` (as opposed to `sendrecv_stale_queued_item_is_skipped_before_
/// dispatch_after_reconnect` above, which covers an item still sitting in the
/// FIFO). Unlike every other in-handler guard in this file's history, this
/// one has a natural preemption point -- the per-tick `tokio::time::sleep` --
/// making a deterministic regression test possible with this crate's
/// single-threaded mock harness.
///
/// cll_a and cll_b share one physical channel (cll_b kept connected
/// throughout to keep it alive). cll_a issues a `CoptDelay` long enough to
/// span several poll ticks; once it is comfortably into its sleep loop
/// (already dispatched, actively executing -- not queued), cll_a is
/// disconnected and immediately reconnected at the SAME `DATA_RATE`,
/// rejoining the identical shared physical channel/poll task but with a
/// freshly-bumped `connect_generation` (the specific scenario a bare
/// `channel_id` check cannot catch, ADR-086's whole premise).
///
/// `DisconnectComLogicalLink`'s own `cancel_link_cops` call immediately
/// cancels the still-executing `CoptDelay` (`PduCopstCancelled`), bypassing
/// `handle_delay` entirely -- this is expected and asserted. The bug this
/// test guards against is `handle_delay` ALSO emitting a status for the same
/// `cop_handle` once it notices the staleness on its own (either mid-sleep,
/// via the per-tick check, or at its terminal check): pre-fix, `handle_delay`
/// had no generation awareness at all, and would have run its sleep loop to
/// completion and unconditionally emitted `PduCopstFinished` for a COP the
/// client was already told was `Cancelled`.
#[tokio::test]
#[serial]
async fn delay_dispatched_item_goes_stale_after_reconnect_on_same_channel() {
    const DELAY_MS: u32 = 400;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // Kept alive (not disconnected) so the shared physical channel -- and its
    // poll task, still busy with cll_a's CoptDelay below -- survives cll_a's
    // disconnect/reconnect. Also used later to isolate handle_delay's
    // per-tick early-break check from its terminal-only recheck.
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );

    let mut events = subscribe(&mut client, cll_a).await;
    let mut events_b = subscribe(&mut client, cll_b).await;

    let delay_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptDelay as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: DELAY_MS,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptDelay) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstExecuting as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == delay_cop_handle))
        .await,
        "the CoptDelay should have started executing"
    );

    // Comfortably into the delay's own per-tick sleep loop (POLL_INTERVAL_MS
    // = 10ms), well short of DELAY_MS: the poll task should be inside
    // handle_delay's own loop, asleep for the current tick, right now.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // A genuinely concurrent disconnect, immediately followed by a reconnect
    // of the SAME cll_handle with the SAME DATA_RATE -- rejoining the same
    // shared physical channel (still kept alive by cll_b) and getting back
    // the identical ChannelId, but a freshly-bumped connect_generation.
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

    // The disconnect's own cancel_link_cops call must have already emitted
    // PduCopstCancelled for the still-executing CoptDelay. Checked BEFORE the
    // absence check below: wait_for_event drains (and discards) every
    // non-matching event it reads while waiting, so checking absence first
    // would silently consume this already-arrived event without ever seeing
    // it.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == delay_cop_handle))
        .await,
        "the still-executing CoptDelay should have received PduCopstCancelled from \
         cancel_link_cops at disconnect time"
    );

    // The reconnect's own ConnectComLogicalLink success legitimately emits
    // exactly one PduCllstOnline for cll_a's new session -- consume it here
    // so nothing else in this test's remaining absence checks could
    // misattribute it.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "the reconnect itself should emit exactly one PduCllstOnline for cll_a's new session"
    );

    // Queue a lightweight CoptDelay(0) on cll_b -- sharing the SAME physical
    // channel/poll task as cll_a's still-stale CoptDelay (connect_count == 1,
    // asserted above) -- to isolate handle_delay's PER-TICK early-break check
    // from its terminal-only recheck (edge-case-hunter finding, caught before
    // this round was committed: the original version of this test only
    // proved the combination of the two checks doesn't double-emit, since
    // cancel_link_cops's own first-wins removal from `primitives` made the
    // terminal recheck alone sufficient to pass the original assertion even
    // with the per-tick check reverted). Without the per-tick check, this
    // queued item would sit behind cll_a's stale delay in the same FIFO until
    // that delay's own natural deadline -- measured from ITS OWN start, still
    // ~250-300ms away at this point -- elapses; the terminal recheck alone
    // would still correctly suppress cll_a's own stray status, but would not
    // free the shared poll task any earlier. With the per-tick check, cll_a's
    // stale delay breaks out within about one poll tick of the reconnect, so
    // cll_b's queued item dispatches promptly.
    let cll_b_delay_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptDelay as i32,
            cop_data: vec![],
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
        .expect("start_com_primitive(CoptDelay) on cll_b should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;

    assert!(
        wait_for_event(&mut events_b, 250, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cll_b_delay_cop_handle))
        .await,
        "cll_b's own CoptDelay(0), sharing the same physical channel's poll task, should finish \
         promptly after the reconnect -- if handle_delay's per-tick generation check did not \
         break cll_a's stale delay early, this item would instead sit blocked behind it in the \
         same FIFO for the remainder of cll_a's original DELAY_MS"
    );

    // Past the full DELAY_MS (measured from the CoptDelay's own start) with
    // margin: handle_delay's own per-tick and/or terminal staleness check
    // must have run by now -- pre-fix, this is where it would have
    // unconditionally emitted a duplicate PduCopstFinished for a cop_handle
    // the client already saw Cancelled.
    tokio::time::sleep(std::time::Duration::from_millis(u64::from(DELAY_MS) + 300)).await;

    assert!(
        !wait_for_event(&mut events, 200, |item| item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == delay_cop_handle))
        .await,
        "no further CopStatus event of any kind should be emitted for the stale CoptDelay's \
         cop_handle -- handle_delay's own staleness detection must not also emit a status for \
         it after cancel_link_cops already won the first-wins race"
    );

    // Close both event streams before shutting down: graceful shutdown waits
    // for in-flight requests, and an open stream never ends -- this test
    // opened two (`events` for cll_a, `events_b` for cll_b), so both must be
    // dropped (a self-inflicted hang here, caught by direct investigation
    // when this test first appeared to hang in CI-equivalent verification,
    // is exactly why this comment calls out "both").
    drop(events);
    drop(events_b);

    server.shutdown().await;
}

/// ADR-095 amendment (2026-07-18, round 2, PR #103 Codex review round 5):
/// the cyclic reap's own CORRECTNESS, not just its promptness, depends on RX
/// freshness -- `run_detached_registrant_maintenance`'s new
/// `rx_freshly_drained` gate. `cll_a` is a created-receive-only
/// (`NumSendCycles = 0`) `NumReceiveCycles = -1` COP with a short (300ms)
/// `CP_CyclicRespTimeout` that has long elapsed by the time `cll_b`
/// (sharing the same physical channel) is mid-`isotp_send`'s
/// STmin/ConsecutiveFrame-block pacing loop -- a hold with NO RX poll
/// anywhere in its own loop body. No response is ever injected for `cop_a`,
/// so any `PduCopstFinished` it receives can only come from the cyclic reap,
/// never a genuine match; observing one DURING cll_b's STmin block directly
/// proves the reap acted on stale state. Before this fix,
/// `run_detached_registrant_maintenance`'s unconditional
/// `reap_expired_cyclic_registrants` call at this site would do exactly
/// that. `cop_a` finishing shortly after cll_b's STmin block completes (once
/// the poll task is no longer inside that unsafe hold) proves the reap is
/// only DELAYED by the fix, not defeated.
#[tokio::test]
#[serial]
async fn receive_only_cyclic_timeout_not_prematurely_reaped_during_a_siblings_stmin_block() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    // cll_a: created-receive-only cyclic COP; never transmits (NumSendCycles
    // = 0) and never matches (no response is ever injected for it) -- its
    // 300ms CP_CyclicRespTimeout is already stale well before cll_b's STmin
    // block (~1.9s) is even mid-flight.
    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CYCLIC_RESP_TIMEOUT, 300_000), // 300 ms
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;
    let mut events_a = subscribe(&mut client, cll_a).await;

    let cop_a = start_send_recv(
        &mut client,
        cll_a,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    let is_finished_for_cop_a = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
    };

    // cll_b: shares cll_a's physical channel, addressed to a different
    // target (0x7A0) so its ISO-TP wire traffic can't be confused with
    // cll_a's own (nonexistent) traffic.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_b");
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7A0).await;
    let mut events_b = subscribe(&mut client, cll_b).await;

    // 111 bytes: FirstFrame + 15 ConsecutiveFrames, same shape as
    // `isotp_stmin_loop_reaps_a_siblings_cancelled_detached_registrant`.
    let payload: Vec<u8> = (1..=111).collect();
    let cop_b = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                num_send_cycles: 1,
                ..Default::default()
            }),
        })
        .await
        .expect("start_com_primitive should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    // cll_a never transmits (NumSendCycles = 0); cll_b's FirstFrame is the
    // channel's first write.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // ECU answers with FlowControl: ContinueToSend, BS=0 (send all 15 CFs
    // back-to-back), STmin=127ms -- ~15 * 127ms =~ 1.9s total CF-sending time.
    let mut fc = 0x7A8_u32.to_be_bytes().to_vec();
    fc.extend_from_slice(&[0x30, 0x00, 0x7F]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &fc, j2534_0404::CAN);

    // Reaching 2 confirms the first ConsecutiveFrame has been written --
    // cll_b's poll task is now inside the STmin pacing loop, with ~14 more
    // CFs (~1.78s) still to go.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    // By now cop_a's own 300ms CP_CyclicRespTimeout deadline is already
    // stale (everything above easily exceeds it); the STmin block itself
    // runs for ~1.9s total, leaving ample time still inside the block to
    // observe whether the reap fired prematurely.
    assert!(
        !wait_for_event(&mut events_a, 900, is_finished_for_cop_a).await,
        "cop_a must NOT finish while cll_b's isotp_send is still deep inside its own \
         STmin/ConsecutiveFrame-block pacing loop -- that loop never polls RX, so the cyclic \
         reap must be gated out (rx_freshly_drained: false) at this site even though cop_a's own \
         deadline has already elapsed"
    );

    // Let cll_b's STmin block finish -- cop_a's cyclic timeout should now be
    // reaped promptly (either by the fix's own safe sites once the hold
    // ends, or by the outer tick once the poll task returns to \
    // poll_channel_events's own select loop).
    assert!(
        wait_for_event(&mut events_a, 2000, is_finished_for_cop_a).await,
        "cop_a's cyclic timeout should be reaped once cll_b's STmin block completes, not \
         deferred indefinitely"
    );

    // Let cll_b's own transfer run out its own timeout naturally before
    // shutdown (no response is ever injected for it either).
    assert!(
        wait_for_event(&mut events_b, 2000, |item| is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_b.cop_handle))
        .await,
        "expected cll_b's CoptSendrecv to finish after its STmin block"
    );

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// ADR-095 amendment (2026-07-18, round 2, PR #103 Codex review round 5)
/// sibling of the STmin test above, covering `wait_for_expected_response_
/// inner`'s RC21/RC23 CHUNKED RETRY SLEEP site specifically (a distinct
/// injection site from the loop-bottom per-iteration flag -- see the
/// dedicated test below for that one): `cop_b`'s receive phase gets NRC 0x21
/// (BusyRepeatRequest), entering the chunked `CP_RC21RequestTime` retry-sleep
/// loop, which never polls RX. `cop_a` is a created-receive-only cyclic COP
/// on the SAME CLL (mirroring `sendrecv_detached_registrant_cancel_reaped_
/// during_a_siblings_rc21_retry_wait`'s single-CLL shape) with a short
/// (100ms) `CP_CyclicRespTimeout` that has already elapsed by the time
/// cop_b's 800ms RC21 wait begins. Before this fix, this chunked sleep's own
/// `run_detached_registrant_maintenance` call ran unconditionally on every
/// chunk -- deleting cop_a mid-wait on RX state that is, by construction,
/// never refreshed anywhere in this loop. cop_a must survive the whole RC21
/// wait and only be reaped once RX is fresh again.
///
/// Fail-without/pass-with note: this test's own "not finished" assertion
/// window (ending well before the 800ms RC21 wait itself completes) is what
/// isolates THIS site's gate from the loop-bottom flag's own gate below --
/// the loop-bottom call for the very iteration that took the 0x21 arm only
/// executes AFTER the whole chunked sleep (plus retransmit) finishes, so it
/// cannot have fired yet at the point this assertion checks.
#[tokio::test]
#[serial]
async fn rc21_chunked_retry_sleep_does_not_prematurely_reap_a_siblings_cyclic_timeout() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    const CP_RC21_COMPLETION_TIMEOUT: u32 = 0x8020;
    const CP_RC21_HANDLING: u32 = 0x8021;
    const CP_RC21_REQUEST_TIME: u32 = 0x8022;
    const CP_RC_BYTE_OFFSET: u32 = 0x8028;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_RC21_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_RC21_REQUEST_TIME, 800_000),
            (CP_RC21_COMPLETION_TIMEOUT, 5_000_000),
            (CP_CYCLIC_RESP_TIMEOUT, 100_000), // 100 ms
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
    let mut events = subscribe(&mut client, cll_handle).await;

    // cop_a: created-receive-only cyclic COP; never transmits, never
    // matches -- its 100ms CP_CyclicRespTimeout is already stale well
    // before cop_b's 800ms RC21 wait even begins.
    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    let is_finished_for_cop_a = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
    };

    // cop_b: triggers NRC 0x21, putting this channel's poll task into the
    // 800ms CP_RC21RequestTime chunked retry sleep.
    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x91],
        0,
        1,
        1,
        vec![expect_positive_response(5)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x22, 0x21]),
        j2534_0404::ISO15765,
    );

    // Give cop_b's RC21 wait a moment to actually begin (the NRC frame must
    // be consumed by a poll pass first) -- by now cop_a's own 100ms deadline
    // is long past.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    assert!(
        !wait_for_event(&mut events, 500, is_finished_for_cop_a).await,
        "cop_a must NOT finish while cop_b's receive-phase loop is still inside the chunked \
         CP_RC21RequestTime retry-sleep loop -- that loop never polls RX, so the cyclic reap \
         must be gated out (rx_freshly_drained: false) at this site even though cop_a's own \
         deadline has already elapsed"
    );

    // Once cop_b's RC21 wait (and its retransmit) completes, the loop
    // returns to a fresh RX poll -- the reap runs with rx_freshly_drained:
    // true shortly after, reaping cop_a promptly.
    assert!(
        wait_for_event(&mut events, 1500, is_finished_for_cop_a).await,
        "cop_a's cyclic timeout should be reaped once cop_b's RC21 wait completes and RX is \
         freshly drained again, not deferred indefinitely"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-095 amendment (2026-07-18, round 2, PR #103 Codex review round 5):
/// isolates the receive-phase loop bottom's own PER-ITERATION gate
/// (`took_rc_retry_arm` in `events.rs`) from the chunked-retry-sleep site's
/// own (separately proven above) gate. Both sites protect the exact same
/// logical iteration and, because `poll_immediately = true` lets the loop
/// re-enter its top with zero sleep, there is no wall-clock window in which
/// only the loop-bottom flag matters -- a purely timing-based proof cannot
/// isolate it. Instead this test uses a durable, state-based signal, mirroring
/// the exact hazard the ADR-095 amendment (round 2) describes: `cop_a` is a
/// created-receive-only cyclic COP with a 300ms `CP_CyclicRespTimeout`, first
/// restarted by an early match at a precisely known moment so it expires
/// strictly DURING (not before) `cop_b`'s later NRC 0x21, which puts this
/// shared CLL's receive-phase loop into its 800ms `CP_RC21RequestTime`
/// chunked sleep (which never polls RX, per the chunked-sleep-site's own
/// gate proven above). A second frame that WOULD satisfy `cop_a`'s own
/// expected-response pattern is injected mid-chunked-sleep -- it sits
/// undrained until the very next
/// top-of-loop `poll_rx_and_check_match` call, which only happens on the
/// FIRST iteration after the 0x21-arm iteration returns (a "normal"
/// iteration). Pre-fix (loop-bottom gate unconditional), the 0x21-arm
/// iteration's own loop-bottom call reaps cop_a on its still-stale deadline
/// BEFORE that iteration -- or any iteration -- ever drains the injected
/// frame, so cop_a can never emit `PduCopstFinished`'s alternative outcome
/// (a `ResultData` match) for it: cop_a finishes wrongly, on stale state.
/// Post-fix, the reap is gated out for the 0x21-arm iteration itself
/// (`rx_freshly_drained: false`, `took_rc_retry_arm` was `true`); the very
/// next NORMAL iteration's own fresh RX poll drains and binds the injected
/// frame FIRST (restarting cop_a's `cyclic_deadline`), so by the time that
/// SAME iteration's own loop-bottom reap call runs (`rx_freshly_drained:
/// true`, `took_rc_retry_arm` now `false`), cop_a's deadline is no longer
/// expired and it survives, having correctly matched.
#[tokio::test]
#[serial]
async fn rc21_retry_arm_iteration_defers_cyclic_reap_to_the_next_normal_iteration() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    const CP_RC21_COMPLETION_TIMEOUT: u32 = 0x8020;
    const CP_RC21_HANDLING: u32 = 0x8021;
    const CP_RC21_REQUEST_TIME: u32 = 0x8022;
    const CP_RC_BYTE_OFFSET: u32 = 0x8028;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_RC21_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_RC21_REQUEST_TIME, 800_000),
            (CP_RC21_COMPLETION_TIMEOUT, 5_000_000),
            (CP_CYCLIC_RESP_TIMEOUT, 300_000), // 300 ms
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
    let mut events = subscribe(&mut client, cll_handle).await;

    // cop_a and cop_b each get a full-SID-specific expected-response pattern
    // (not the generic single-byte `expect_positive_response` helper other
    // tests in this file use) so the frame injected below for cop_a cannot
    // be ambiguously claimed by cop_b's own actively-waiting registrant
    // instead -- both share the same CLL/unique_resp mapping, so a generic
    // "any 0x62-prefixed frame" pattern would match either.
    let cop_a_expected = ExpectedResponseData {
        response_type: 0,
        acceptance_id: 9,
        mask_data: vec![0xFF, 0xFF, 0xFF],
        pattern_data: vec![0x62, 0xF1, 0x90],
        unique_resp_ids: vec![],
    };
    let cop_b_expected = ExpectedResponseData {
        response_type: 0,
        acceptance_id: 5,
        mask_data: vec![0xFF, 0xFF, 0xFF],
        pattern_data: vec![0x62, 0xF1, 0x91],
        unique_resp_ids: vec![],
    };

    // cop_a: created-receive-only cyclic COP. Its CP_CyclicRespTimeout is
    // deliberately 300ms (comfortably longer than the setup steps below)
    // rather than pre-expired at creation -- an EARLY match, injected right
    // away, restarts its deadline at a precisely known moment so it expires
    // strictly DURING cop_b's later 800ms RC21 wait, not before cop_b even
    // starts (which would let cop_a be reaped by the routine, unconditional
    // per-tick sweep well before the interesting part of this test begins).
    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![cop_a_expected],
    )
    .await
    .expect("start_com_primitive should succeed");
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x00]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 9
        ))
        .await,
        "cop_a's early match should restart its cyclic_deadline at a known moment"
    );

    // cop_b: triggers NRC 0x21, putting this channel's poll task into the
    // 800ms CP_RC21RequestTime chunked retry sleep. By the time this wait
    // begins, only a fraction of cop_a's 300ms (restarted) deadline has
    // elapsed -- it will expire strictly inside this 800ms window.
    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x91],
        0,
        1,
        1,
        vec![cop_b_expected],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x22, 0x21]),
        j2534_0404::ISO15765,
    );

    // Give cop_b's RC21 wait a moment to actually begin (the NRC frame must
    // be consumed by a poll pass first) before injecting cop_a's second
    // match -- otherwise it could land in an ordinary pre-RC21-wait poll
    // pass instead of sitting undrained through the wait, as this test needs.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Inject a second frame that satisfies cop_a's own expected-response
    // pattern WHILE cop_b's chunked RC21 sleep is in progress. This loop
    // never polls RX, so the frame sits undrained until the next top-of-loop
    // `poll_rx_and_check_match` call -- necessarily on the FIRST iteration
    // after the 0x21-arm iteration returns. cop_a's (restarted) deadline
    // will expire well before this 800ms window ends.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );

    // cop_a must not finish anywhere in this window (which spans past its
    // own now-expired deadline, well before cop_b's RC21 wait ends): pre-fix,
    // it would be wrongly reaped by the 0x21-arm iteration's own loop-bottom
    // call, on stale state, before ever seeing the frame just injected for
    // it.
    assert!(
        !wait_for_event(&mut events, 650, is_finished).await,
        "cop_a must not be reaped mid-RC21-wait on stale state -- the frame injected above \
         would have satisfied it, so a premature PduCopstFinished here proves the loop-bottom \
         reap ran on the 0x21-arm iteration itself rather than being deferred to the next \
         normal iteration"
    );

    // Post-fix: the next normal iteration's own fresh RX poll drains and
    // binds the injected frame FIRST, restarting cop_a's cyclic_deadline
    // before that same iteration's own (now-safe) reap call ever evaluates
    // it -- cop_a survives, having matched.
    assert!(
        wait_for_event(&mut events, 1500, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 9
        ))
        .await,
        "cop_a should accept the frame injected during cop_b's RC21 wait once RX is fresh again \
         -- its cyclic_deadline should be restarted, not reaped out from under the match"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-095 amendment (2026-07-18, round 2, PR #103 Codex review round 5),
/// the fifth injection site: `handle_delay` previously had NO
/// `run_detached_registrant_maintenance` call at all, despite polling RX and
/// dispatching tester-present every tick -- a client-issued `CoptDelay`
/// starved both detached-registrant duties for its entire configured
/// duration. `cop_a` detaches to tier-2 via an accepted first match, then a
/// long `CoptDelay` occupies this channel's poll task; `cop_a`'s
/// cancellation is issued mid-delay and must be reaped well within the
/// delay's own window, not only once the delay itself finishes.
#[tokio::test]
#[serial]
async fn handle_delay_reaps_a_siblings_cancelled_detached_registrant() {
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
    let mut events = subscribe(&mut client, cll_handle).await;

    // cop_a: IS-CYCLIC, detaches to tier-2 on its first accepted match.
    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
        ))
        .await,
        "cop_a should accept its first response and detach"
    );

    // A 1000ms CoptDelay occupies this channel's poll task -- before this
    // fix, handle_delay never called run_detached_registrant_maintenance at
    // all, so a cancellation issued mid-delay would only be reaped once the
    // delay itself finished.
    queue_delay(&mut client, cll_handle, 1000).await;

    // Give the delay a moment to actually start.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("cancel_com_primitive on a detached cop should succeed");

    let cancel_issued_at = std::time::Instant::now();
    assert!(
        wait_for_event(&mut events, 300, is_cancelled).await,
        "cop_a's cancellation should be reaped promptly even while this channel's poll task is \
         inside handle_delay's own CoptDelay hold -- before this fix, handle_delay never called \
         run_detached_registrant_maintenance at all"
    );
    assert!(
        cancel_issued_at.elapsed() < std::time::Duration::from_millis(700),
        "cop_a's cancellation took {:?}, which is suspiciously close to the CoptDelay's 1000ms \
         window -- expected it to be reaped within roughly one poll interval instead",
        cancel_issued_at.elapsed()
    );

    // Let the CoptDelay itself finish naturally before shutdown.
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "expected the CoptDelay to finish after its own 1000ms window"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-095 amendment (2026-07-18, final revision, PR #103 Codex review round
/// 6): the cyclic reap's `rx_drained` gate must mean EXHAUSTIVE drain, not
/// merely "a poll ran" -- `poll_rx_inner` makes exactly ONE bounded
/// `PassThruReadMsgs(..., MAX_POLL_MESSAGES = 8, ...)` call per invocation,
/// never looping until the adapter reports empty. This test proves the gap
/// the round-5 fix (`rx_freshly_drained`, gating on "did a poll run this
/// iteration") left open: with a backlog of 9+ queued frames, one poll pass
/// drains only the first 8, proving nothing about whether the 9th (or
/// later) frame -- possibly the very one that would satisfy a receive-only
/// cyclic registrant's `CP_CyclicRespTimeout` -- is still queued.
///
/// This specifically exercises `run_due_tick_duties`'s own DIRECT
/// `reap_expired_cyclic_registrants` call (required test 4): `cop_a` is a
/// created-receive-only (`NumSendCycles = 0`) `NumReceiveCycles = -1` COP
/// with no other CLL and no other poll-task hold in play, so its ONLY
/// maintenance path is the idle outer tick, `run_due_tick_duties` itself --
/// this is the site the ADR calls out as a pre-existing hole even before
/// this whole amendment, never routed through
/// `run_detached_registrant_maintenance` at all.
///
/// Shape: `cop_a`'s `CP_CyclicRespTimeout` (100ms) is set short relative to
/// `MAX_POLL_MESSAGES`-sized batches at `POLL_INTERVAL_MS` (10ms) cadence.
/// Immediately after `cop_a` is created, a burst of 240 filler frames (none
/// matching `cop_a`'s `ExpectedResponseStructure`) plus one final, MATCHING
/// 241st frame are injected in one synchronous call sequence (no `.await`
/// between injections, so the poll task cannot interleave and drain any of
/// them mid-burst). Draining 240 filler frames alone takes 30 full
/// (`MaybeMore`) poll passes (~300ms) before the queue is short enough to
/// report `Drained` -- `cop_a`'s 100ms deadline elapses well inside that
/// window, while the matching 241st frame is still unread. If the cyclic
/// reap gate merely checked "a poll ran" (the round-5 behavior), it would
/// incorrectly reap `cop_a` on one of those early `MaybeMore` passes despite
/// the real match sitting queued right behind. With this fix, the reap
/// stays gated out until a `Drained` pass -- which is also the exact pass
/// that finally drains and binds the 241st, matching frame, restarting
/// `cop_a`'s own deadline in the same pass the reap would otherwise have
/// fired in.
///
/// The 40-frame/20ms-deadline shape this test originally used checked "not
/// yet reaped" at 40ms via the unfiltered `is_finished` predicate (matches
/// ANY cop's `PduCopstFinished`, not just cop_a's). That predicate choice
/// was the actual bug: `set_unique_resp_table_and_promote` (called before
/// `subscribe` below) issues its own `CoptUpdateparam` COP, which executes
/// and finishes essentially immediately -- its `Executing`/`Finished`
/// events sit queued in the CLL's own event buffer (populated regardless of
/// subscriber presence) from before `subscribe` is ever called, and are
/// replayed as the first events any new subscription reads. Delivering that
/// backlog over the gRPC stream measured consistently at ~40ms in this
/// environment, so the ORIGINAL 40ms checkpoint was unknowingly racing that
/// unrelated COP's own stale terminal event, not cop_a's -- explaining both
/// the historical CI failure and the 4% isolated-rerun rate (see
/// `docs/implementation-notes.md`'s "Test-suite reliability: past
/// flaky-test root causes" section) as a race
/// against backlog-replay latency, not against the exhaustive-drain timing
/// this test is meant to exercise. Confirmed by direct instrumentation
/// (temporary `eprintln!`s in both this test and `reap_expired_cyclic_
/// registrants`/`poll_rx_inner`, since reverted): cop_a's own registrant
/// (`RegistrantTier::ReceiveOnly`) never satisfied `is_cyclic_reap_sound`
/// during the run, proving the "cop_a must not be reaped" assertion was
/// actually observing the promote step's leftover `Finished` event instead.
/// Fixed by filtering every `is_finished` check in this test to cop_a's own
/// `cop_handle`, exactly like the neighbouring `cyclic_reap_defers_until_
/// the_uudt_companion_channels_watermark_also_catches_up` test already does
/// via its own `is_finished_for_cop_a` closure -- not a production bug.
///
/// Also scaled up 6x from the original 40-frame/20ms-deadline/40ms-
/// checkpoint shape (matching the ~10-tick-margin ratio the neighbouring
/// companion test above already uses successfully), independently of the
/// filter fix: the original checkpoint left only one `POLL_INTERVAL_MS`
/// tick (10ms) of margin before the 5-pass, 50ms full-drain boundary, and
/// the "deadline was restarted" check left only 5ms of margin before the
/// 20ms deadline -- both thin enough to be sensitive to ordinary scheduling
/// jitter on top of the filter bug above.
#[tokio::test]
#[serial]
async fn receive_only_cyclic_reap_gated_by_exhaustive_drain_not_just_a_poll_running() {
    const MAX_POLL_MESSAGES: usize = 8;
    const FILLER_FRAMES: usize = 30 * MAX_POLL_MESSAGES; // 30 full MaybeMore batches (~300ms to drain).

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CYCLIC_RESP_TIMEOUT, 100_000), // 100 ms -- elapses well inside the ~300ms backlog window below.
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    let is_finished_for_cop_a = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
    };

    // Synchronous burst, no `.await` between calls: FILLER_FRAMES (240)
    // non-matching frames, then one final matching frame -- exactly the
    // "9th (or later) frame is the matching one" shape the fix's own
    // correctness depends on, scaled up so the backlog persists across
    // several poll passes rather than just one.
    for _ in 0..FILLER_FRAMES {
        server.backdoor.inject_rx(
            MOCK_CHANNEL_ID,
            &can_frame(0x7E8, &[0x00, 0x00, 0x00, 0x00]), // does not match the [0x62] pattern
            j2534_0404::ISO15765,
        );
    }
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );

    // Within this window, at most 20 of the 30 filler-only poll passes have
    // run -- 10 ticks (~100ms) of margin short of fully draining the 240
    // filler frames -- so every pass so far has seen a full
    // MAX_POLL_MESSAGES batch (MaybeMore) -- cop_a's own 100ms deadline has
    // long since elapsed by this point, so a reap gated merely on "a poll
    // ran" would already have fired.
    assert!(
        !wait_for_event(&mut events, 200, is_finished_for_cop_a).await,
        "cop_a must not be reaped while every poll pass so far has drained a full \
         MAX_POLL_MESSAGES batch (MaybeMore) -- proving nothing about whether the queued \
         matching frame remains undrained -- even though cop_a's own CP_CyclicRespTimeout \
         deadline already elapsed"
    );

    // The matching frame is eventually drained (once the filler backlog
    // clears) and delivered as a normal match.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 9
        ))
        .await,
        "the matching frame queued behind the filler backlog must still be delivered once a \
         Drained poll pass reaches it"
    );

    // cop_a must still be alive for a window comfortably inside its own
    // freshly-restarted 100ms deadline (proving `cyclic_deadline` was pushed
    // forward to match-time + 100ms, not left stale at its original,
    // already-elapsed pre-match value -- a stale deadline would have let the
    // very next tick reap it immediately). Deliberately short: this test
    // does not assert indefinite survival, only that the restart actually
    // happened.
    assert!(
        !wait_for_event(&mut events, 50, is_finished_for_cop_a).await,
        "cop_a's cyclic_deadline should have been restarted by the just-delivered match, not \
         left stale from before the backlog was drained"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-101 Decision §E (PR #103 Codex review round 7): the cyclic reap must
/// be sound against BOTH channels of a dual-channel CLL, not just the
/// PRIMARY channel's own drain state -- `reap_expired_cyclic_registrants`
/// only ever runs from the primary channel's poll task, and round 6's own
/// `rx_drained` gate had zero visibility into a UUDT companion channel's
/// entirely independent adapter queue (ADR-046). This test backlogs the
/// COMPANION channel (not the primary, unlike every prior round's own test in
/// this file) with the same "N full MAX_POLL_MESSAGES batches" technique
/// `receive_only_cyclic_reap_gated_by_exhaustive_drain_not_just_a_poll_running`
/// (round 6) uses on the primary: `cop_a`'s CP_CyclicRespTimeout elapses
/// while the PRIMARY channel is already exhaustively drained (no backlog of
/// its own) but the COMPANION channel is still working through its own
/// backlog. Under the pre-Decision-§E scheme (gating on the CALLING task's --
/// the primary's -- own `rx_drained` alone), the primary's own next tick
/// would already satisfy that gate and reap `cop_a` regardless of the
/// companion's queue state entirely; this proves the fix defers the reap
/// until the companion's own watermark also catches up, then completes it
/// once it does.
#[tokio::test]
#[serial]
async fn cyclic_reap_defers_until_the_uudt_companion_channels_watermark_also_catches_up() {
    const COMPANION_CHANNEL_ID: u32 = 2;
    const MAX_POLL_MESSAGES: usize = 8;
    const COMPANION_FILLER_FRAMES: usize = 20 * MAX_POLL_MESSAGES; // 20 full MaybeMore batches (~200ms to drain).

    let server = TestServer::start_with_can_mode(Some("dual-channel")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CYCLIC_RESP_TIMEOUT, 20_000), // 20 ms -- elapses well before the companion's ~200ms backlog drain.
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
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // Backlog the COMPANION channel with non-matching filler frames BEFORE
    // creating cop_a -- synchronous burst, no `.await` between injections, so
    // ordering is structural rather than timing-lucky (same discipline as
    // round 6's own primary-channel backlog test). None of these frames
    // start with 0x62, so they cannot accidentally satisfy cop_a's own
    // expected-response pattern.
    for _ in 0..COMPANION_FILLER_FRAMES {
        server.backdoor.inject_rx(
            COMPANION_CHANNEL_ID,
            &can_frame(0x5E8, &[0x00, 0x00]),
            j2534_0404::CAN,
        );
    }

    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    let is_finished_for_cop_a = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
    };

    // The PRIMARY channel has no backlog of its own, so it stamps a
    // post-deadline watermark on its very next idle tick (well under 20ms
    // after cop_a's own deadline). The COMPANION channel is still working
    // through its 160-frame backlog and cannot report a post-deadline
    // watermark for several more poll passes (~200ms total) -- proving the
    // reap defers to the companion, not just the primary.
    assert!(
        !wait_for_event(&mut events, 100, is_finished_for_cop_a).await,
        "cop_a must not be reaped while the companion channel's own drain watermark is still \
         stale -- the primary channel alone having proven an exhaustive drain past the deadline \
         must not be sufficient for a dual-channel CLL"
    );

    // Once the companion channel finishes draining its backlog, its own
    // watermark catches up past cop_a's deadline too -- now BOTH channels are
    // sound, and the reap proceeds.
    assert!(
        wait_for_event(&mut events, 2000, is_finished_for_cop_a).await,
        "cop_a should be reaped once the companion channel's own watermark also catches up to \
         the deadline"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-101 Decision §E's tier-1 extension (same round-7 finding): the
/// identical cross-channel soundness gap exists one tier up, in
/// `wait_for_expected_response_inner`'s own finite-count receive-phase loop.
/// A `NumReceiveCycles = 1` COP's `CP_P2Max` deadline can be reached while an
/// in-time, matching response is still sitting queued -- undrained -- behind
/// a backlog on the CLL's UUDT companion channel. Pre-fix, the loop's
/// unconditional `!no_deadline && now >= deadline` break would end the wait
/// right there, emitting a spurious `PduErrEvtRxTimeout`, and the matching
/// frame would later be discarded as unbound (ADR-100 Decision §5) once the
/// companion eventually polls it. Post-fix, the loop takes a grace pass
/// instead of breaking until the companion's own watermark also catches up
/// to the deadline, so the COP completes normally with the companion-
/// delivered match instead.
#[tokio::test]
#[serial]
async fn tier1_wait_takes_a_grace_pass_for_a_companion_delivered_match_still_queued_at_the_deadline()
 {
    const COMPANION_CHANNEL_ID: u32 = 2;
    const MAX_POLL_MESSAGES: usize = 8;
    const COMPANION_FILLER_FRAMES: usize = 5 * MAX_POLL_MESSAGES; // 5 full MaybeMore batches (~50ms to drain).

    let server = TestServer::start_with_can_mode(Some("dual-channel")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 20_000), // 20 ms window -- shorter than the companion's ~50ms backlog drain.
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
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // Backlog the companion channel with filler frames, then the actual
    // matching response as the LAST frame in the burst -- so it sits
    // undrained for several companion poll passes (~50ms), well past this
    // COP's own 20ms CP_P2Max window.
    for _ in 0..COMPANION_FILLER_FRAMES {
        server.backdoor.inject_rx(
            COMPANION_CHANNEL_ID,
            &can_frame(0x5E8, &[0x00, 0x00]),
            j2534_0404::CAN,
        );
    }
    server.backdoor.inject_rx(
        COMPANION_CHANNEL_ID,
        &can_frame(0x5E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::CAN,
    );

    let cop = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        1,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut matched = false;
    let mut saw_rx_timeout = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            match &item.data {
                Some(event_item::Data::ResultData(result)) if result.acceptance_id == 9 => {
                    matched = true;
                }
                Some(event_item::Data::ErrorData(error))
                    if *error
                        == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32 =>
                {
                    saw_rx_timeout = true;
                }
                _ => {}
            }
            is_finished(item)
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop.cop_handle)
        })
        .await,
        "the COP should finish once the companion channel's backlog finally delivers the queued \
         match"
    );
    assert!(
        matched,
        "the companion-delivered response should have been accepted as cop_a's match"
    );
    assert!(
        !saw_rx_timeout,
        "the tier-1 wait's deadline-break must take a grace pass for the companion channel's \
         own watermark instead of timing out while an in-time response is still queued behind \
         its backlog"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-101 Decision §E correction (Codex review of PR #103, round 8):
/// `companion_caught_up` proves the companion's own writeback merge has
/// landed in LIVE registrant state (the watermark is only stamped after that
/// merge), but the tier-1 wait loop's own local `matches_got` was sampled
/// EARLIER in the same iteration, via `poll_rx_and_check_match`'s
/// top-of-loop call -- a point that can predate that same merge, since the
/// top-of-loop sample, the cancellation/staleness check, and the watermark
/// read are three separate `logical_links`/`drain_watermarks` lock
/// acquisitions. Landing a companion merge deterministically inside that one
/// specific, sub-millisecond, largely-uncontended-lock window is the same
/// class of "structurally infeasible to construct with this crate's
/// `current_thread` `#[tokio::test]` harness and real timers" window already
/// recorded for several other narrow lock-acquisition races in
/// `docs/implementation-notes.md` (the round-4/5/6/8 ADR-086 notes) -- there
/// is no controllable pause/gate hook here to force the companion's own
/// merge-then-stamp sequence to land strictly between this loop's own two
/// reads, and an uncontended `tokio::sync::Mutex::lock().await` resolves
/// without a genuine `Pending`-yielding point, so nothing forces the two
/// tasks to interleave inside that specific gap on demand.
///
/// This test instead follows the fallback this fix's own brief endorses for
/// that class of window: reliably land the companion's match delivery
/// AT-OR-AFTER the deadline, repeated many times (`ROUNDS`, phase-swept via
/// a varying per-round filler count) in a single wait's lifetime, to try to
/// raise the odds of the narrow race actually landing at least once across
/// the run via real scheduler/lock-contention jitter (unlike
/// `tier1_wait_takes_a_grace_pass_for_a_companion_delivered_match_
/// still_queued_at_the_deadline` above, which only has ONE such deadline
/// crossing and therefore only one chance).
///
/// Fail-without/pass-with proof attempted (manual, not committed): with the
/// round-8 recheck reverted to the unconditional
/// `if companion_caught_up { break; }` this ADR corrects, this test was run
/// four times, including a 60-round/phase-swept variant -- it passed every
/// time, never reproducing the spurious `PduErrEvtRxTimeout`. Each run's
/// reported wall-clock duration was identical to the millisecond across
/// repeats, indicating this crate's `current_thread` `#[tokio::test]`
/// runtime with real (unmocked) timers produces a highly reproducible
/// relative task-scheduling order for a given test body, not the kind of
/// run-to-run jitter this approach needs to eventually land the race by
/// chance. This confirms (empirically, not just by the structural argument
/// above) that this specific window falls in the same infeasible-without-a-
/// dedicated-pause/gate-hook class as `docs/implementation-notes.md`'s
/// existing round-4/5/6/8 notes -- so this test does NOT, in practice,
/// distinguish the fixed and reverted code, and must not be read as a
/// fail-without/pass-with proof of the round-8 fix itself. It is retained as
/// a soak test proving the round-7 grace-pass mechanism (`awaiting_companion_
/// drain`, per-round deadline reset, watermark catch-up) stays correct
/// across many sequential companion races within one wait, with no dropped
/// or double-counted round. The round-8 recheck's own correctness instead
/// rests on `check_match_against_baseline`'s existing direct unit tests
/// (`check_match_against_baseline_tests`, below `poll_rx_and_check_match`),
/// which already cover exactly the three cases this recheck depends on --
/// unchanged baseline with no `pending_rc` reports `NoMatch` (no unobserved
/// delta), `matches_got` advanced past baseline reports `Matched` (an
/// unobserved delta), and a `pending_rc` set with `matches_got` unchanged
/// reports `PendingRc` (also an unobserved delta) -- since this fix reuses
/// that exact pure predicate rather than introducing new logic at the break
/// site.
#[tokio::test]
#[serial]
async fn tier1_wait_recheck_survives_many_deadline_vs_companion_watermark_races_in_one_wait() {
    const COMPANION_CHANNEL_ID: u32 = 2;
    const MAX_POLL_MESSAGES: usize = 8;
    const ROUNDS: usize = 60;

    let server = TestServer::start_with_can_mode(Some("dual-channel")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 15_000), // 15ms -- shorter than each round's ~30ms companion drain.
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
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // Queue ROUNDS separate companion backlogs up front, one synchronous
    // burst with no `.await` between injections (structural ordering, not
    // timing-lucky, same discipline as the round-6/round-7 tests above).
    // Each round is deliberately slower to drain (2-4 full MAX_POLL_MESSAGES
    // batches, swept round-to-round to sweep the relative phase between this
    // wait's own ~10ms poll ticks and the companion's own drain-completion
    // instant through a range of offsets, rather than pinning it at one
    // fixed relative phase for all 60 rounds) than this COP's own 15ms
    // CP_P2Max, so every round forces the primary loop's deadline check to
    // reach the `companion_caught_up` gate, with the companion's watermark
    // flipping past the (freshly-reset) deadline exactly once per round.
    for round in 0..ROUNDS {
        let filler_this_round = (2 + round % 3) * MAX_POLL_MESSAGES; // sweeps 16/24/32 frames.
        for _ in 0..filler_this_round {
            server.backdoor.inject_rx(
                COMPANION_CHANNEL_ID,
                &can_frame(0x5E8, &[0x00, round as u8]), // does not match the [0x62] pattern
                j2534_0404::CAN,
            );
        }
        server.backdoor.inject_rx(
            COMPANION_CHANNEL_ID,
            &can_frame(0x5E8, &[0x62, 0xF1, 0x90, round as u8]),
            j2534_0404::CAN,
        );
    }

    let cop = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        ROUNDS as i32,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut match_count = 0u32;
    let mut saw_rx_timeout = false;
    assert!(
        wait_for_event(&mut events, 10_000, |item| {
            match &item.data {
                Some(event_item::Data::ResultData(result)) if result.acceptance_id == 9 => {
                    match_count += 1;
                }
                Some(event_item::Data::ErrorData(error))
                    if *error
                        == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32 =>
                {
                    saw_rx_timeout = true;
                }
                _ => {}
            }
            is_finished(item)
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop.cop_handle)
        })
        .await,
        "the COP should finish once all {ROUNDS} companion-delivered matches are counted, not \
         get stuck mid-way"
    );
    assert_eq!(
        match_count, ROUNDS as u32,
        "every round's companion-delivered match should have been counted toward this COP's \
         NumReceiveCycles -- a lost round would mean a bad break ended the wait early"
    );
    assert!(
        !saw_rx_timeout,
        "the tier-1 wait's deadline-break recheck must survive every one of {ROUNDS} \
         deadline-vs-companion-watermark races across this run without reporting a spurious \
         PduErrEvtRxTimeout for an already companion-delivered match (ADR-101 Decision §E \
         correction, round 8)"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-100 round-9 Finding-1 correction: `tier` is keyed on
/// `wait.created_receive_only` alone, not the narrower `-1`-only
/// `is_receive_only_cyclic` -- a created-receive-only COP with a FINITE
/// `NumReceiveCycles` (or `-2`) is tier-2 (`RegistrantTier::ReceiveOnly`)
/// from creation, exactly like the `-1` subtype, not
/// `RegistrantTier::ActiveSendReceive`. Verified here via the OBSERVABLE
/// consequence: an OLDER tier-2 monitor (registered first, a vacuous/broad
/// descriptor) and a NEWER finite-`N` created-receive-only COP (a specific
/// descriptor) both live on the same CLL; a frame matching BOTH descriptors
/// must be claimed by the OLDER monitor (registration-order precedence
/// WITHIN tier 2, ADR-100 Decision §3's "Resolved (a)"), not stolen by the
/// newer COP acting as tier-1 (which would rank it in step 2, ahead of
/// EITHER tier-2 registrant, per Decision §3's precedence table).
///
/// Fail-without/pass-with proof: with `wait.created_receive_only` reverted
/// to `is_receive_only_cyclic` at this registrant's `tier` field (`events.rs`,
/// `wait_for_expected_response`), the finite-`N` COP below is
/// (mis)classified `ActiveSendReceive`, so its own non-vacuous descriptor
/// wins step 2 of the precedence table before the older monitor's tier-2
/// scan (step 5) ever runs -- the frame is delivered with the finite-`N`
/// COP's `acceptance_id` (200) instead of the older monitor's (100), and
/// this test's assertion fails. Verified manually: reverting just that one
/// line reproduces the failure (`got == Some(200)`); restoring it passes.
#[tokio::test]
#[serial]
async fn receive_only_finite_n_created_cop_is_tier_two_registration_order_precedence() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 2_000_000),
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
    let mut events = subscribe(&mut client, cll_handle).await;

    // Older tier-2 monitor: created-receive-only, NumReceiveCycles = -1
    // (already tier-2 from creation both before and after this correction --
    // used here purely as the stable, older reference registrant), a
    // vacuous (empty mask/pattern) descriptor that claims any frame.
    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![ExpectedResponseData {
            response_type: 0,
            acceptance_id: 100,
            mask_data: vec![],
            pattern_data: vec![],
            unique_resp_ids: vec![],
        }],
    )
    .await
    .expect("start_com_primitive should succeed");

    // Newer finite-N (N=2) created-receive-only COP with a SPECIFIC
    // descriptor -- pre-fix, this was misclassified ActiveSendReceive
    // (tier-1); post-fix, ReceiveOnly (tier-2) from creation.
    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        2,
        vec![expect_positive_response(200)],
    )
    .await
    .expect("start_com_primitive should succeed");

    // Give the poll task time to register both (structural FIFO dispatch
    // order on this one physical channel's TX queue guarantees the OLDER
    // monitor above is registered first; NumSendCycles = 0 skips
    // transmission for both, so there is no `wait_for_written_count` signal
    // to block on instead).
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    // Matches BOTH the older monitor's vacuous descriptor and the newer
    // COP's specific one.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );

    let mut got: Option<u32> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                got = Some(result.acceptance_id);
                return true;
            }
            false
        })
        .await,
        "the frame should be delivered to one of the two registrants"
    );
    assert_eq!(
        got,
        Some(100),
        "the OLDER tier-2 monitor (registered first) should claim this frame -- a stray tier-1 \
         classification of the newer finite-N created-receive-only COP would instead let IT \
         steal the frame via step 2's tier-1 non-vacuous precedence"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-100 round-9 Finding-1 correction, second consequence: `rc_cfg` is
/// keyed on `wait.created_receive_only` alone too -- a finite-`N`
/// created-receive-only COP gets `rc_cfg: None` (no RC78/21/23 auto-handling,
/// per the 7F clause (negative-response handling does not apply to
/// receive-only ComPrimitives), for the WHOLE tier, not just the `-1`
/// subtype). Verified via the client-visible behavior change ADR-100's
/// correction calls out explicitly: a pending-RC (`0x78`) frame no longer
/// extends this COP's wait.
///
/// Fail-without/pass-with proof: `bind_registrant`'s `detect_pending_rc` call
/// is itself gated on `r.tier == ActiveSendReceive` (only the
/// `Tier1NonVacuous` scan ever consults `rc_cfg` at all --
/// `CopRegistrant::rc_cfg`'s own doc comment: "`Some` only for a tier-1
/// registrant"), so `rc_cfg`'s OWN field value is provably inert in
/// isolation once `tier` is correctly `ReceiveOnly` -- reverting only the
/// `rc_cfg` line while leaving `tier` fixed produces no observable
/// difference (verified manually: this test still passes). The two lines
/// are jointly necessary to reproduce the historical bug, exactly as they
/// were jointly driven by the same `is_comparam_timed_receive_only`
/// condition pre-fix: reverting BOTH the `tier` and `rc_cfg` lines together
/// (the literal pre-fix shape) re-admits this registrant into the
/// `Tier1NonVacuous` scan AND gives it a non-`None` `rc_cfg`, reproducing
/// the failure (no `PduCopstFinished` within 700ms); restoring both passes.
/// Verified manually.
///
/// ADR-182 update: this test originally relied on `CP_P2Max` to time the
/// finite-`N` COP out (pre-ADR-182, a finite-`N` created-receive-only COP
/// ran inline, `CP_P2Max`-governed, exactly like the pre-ADR-100
/// `Tier1NonVacuous` shape this test's fail-without proof reproduces).
/// Post-ADR-182, this COP detaches to tier-2 immediately and `CP_P2Max` no
/// longer governs it at all -- `CP_CyclicRespTimeout` does, so the CLL now
/// configures that ComParam instead and the timing assertion below is
/// rebased on it; the `rc_cfg: None`/pending-RC assertion itself is
/// unaffected by ADR-182 (it is about attribution, not timing).
#[tokio::test]
#[serial]
async fn receive_only_finite_n_created_cop_rc_cfg_none_pending_rc_does_not_extend_wait() {
    const CP_RC_BYTE_OFFSET: u32 = 0x8028;
    const CP_RC78_HANDLING: u32 = 0x8027;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CYCLIC_RESP_TIMEOUT, 300_000), // 300ms (ADR-182)
            (CP_RC78_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 2),
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
    let mut events = subscribe(&mut client, cll_handle).await;

    // Finite-N (N=2) created-receive-only COP; cop_data's first byte (0x22)
    // becomes rc_cfg.request_sid regardless of NumSendCycles == 0 (derived
    // at the RPC layer from the raw request, ADR-100 Decision §3 resolved
    // (b)) -- irrelevant post-fix since rc_cfg itself is None, but pins the
    // exact pre-fix shape this test's fail-without proof exercises.
    let cop = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        2,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    // A pending-RC (0x78) frame echoing the request SID (0x22) -- would
    // extend the wait under the pre-fix `rc_cfg: Some(..)` classification.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x22, 0x78]),
        j2534_0404::ISO15765,
    );

    let mut saw_rx_timeout = false;
    assert!(
        wait_for_event(&mut events, 700, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32
            ) {
                saw_rx_timeout = true;
            }
            is_finished(item)
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop.cop_handle)
        })
        .await,
        "the COP should finish at its configured CP_CyclicRespTimeout deadline (~300ms, \
         ADR-182) -- a pending-RC extension (pre-fix rc_cfg: Some) would push this well past \
         this 700ms bound"
    );
    assert!(
        saw_rx_timeout,
        "with rc_cfg: None, the 0x78 frame matches no descriptor and is discarded as unbound -- \
         the COP still times out normally with 0 of 2 required matches"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-095 amendment's round-9 correction ("a reap injected inside a hold
/// can never reap the registrant servicing that same hold") / ADR-100 round-9
/// Finding-1's own "Safety-invariant interaction" note originally covered a
/// finite-`N` created-receive-only COP that was tier-2 (`ReceiveOnly`) from
/// the START of its wait, yet -- per ADR-100 Decision §4/Out-of-scope's
/// Stage 3 boundary at the time -- still ran inline/blocking in
/// `wait_for_expected_response_inner`'s own loop, on this channel's poll
/// task, for the whole of that wait.
///
/// **ADR-182 update:** that Stage 3 boundary no longer applies to this
/// shape -- a finite-`N` created-receive-only COP now detaches to tier-2
/// immediately at creation, exactly like the `-1` subtype already did, and
/// never enters `wait_for_expected_response_inner` at all. Cancelling it is
/// therefore now handled entirely by `reap_cancelled_detached_registrants`'s
/// per-tick sweep -- the SAME mechanism, and the SAME already-proven-sound
/// `Some(r.cop_handle) != currently_executing` exclusion, the `-1` subtype's
/// own detach-cancel tests already exercise (e.g.
/// `sendrecv_is_cyclic_cancel_after_detach_removes_registrant`,
/// `receive_only_cyclic_cancel_works_after_detach_with_timeout_configured`)
/// -- the specific inline-loop-vs-reap race window this test was originally
/// written to regress no longer exists for this shape at all (there is no
/// inline loop left to race against). This test is kept, renamed, and
/// simplified to cover the new, simpler invariant directly: cancelling a
/// detached, un-deadlined (`CP_CyclicRespTimeout` unset) finite-`N`
/// registrant still yields exactly one `PduCopstCancelled`, never a
/// spurious `PduErrEvtRxTimeout`.
#[tokio::test]
#[serial]
async fn receive_only_finite_n_cancel_after_detach_yields_single_cancelled_status() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // A finite-N (N=2) created-receive-only COP: tier-2 from creation
    // (ADR-100 round-9 Finding-1) and detached immediately (ADR-182) --
    // `CP_CyclicRespTimeout` is left unset (defaults to 0/disabled), so this
    // registrant has no deadline of its own and would otherwise sit alive
    // until `CancelComPrimitive` or its target count is reached.
    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        2,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");

    // Give the poll task time to enter the receive-phase wait (NumSendCycles
    // = 0 skips transmission, so the receive phase begins almost
    // immediately).
    tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    let mut saw_rx_timeout = false;
    let mut cancelled_count = 0u32;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if is_cancelled(item) {
                cancelled_count += 1;
            }
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32
            ) {
                saw_rx_timeout = true;
            }
            is_cancelled(item)
        })
        .await,
        "cancelling a detached finite-N created-receive-only COP should still cancel it"
    );

    // Give any spurious follow-up event a generous extra window to show up.
    assert!(
        !wait_for_event(&mut events, 500, |item| {
            if is_cancelled(item) {
                cancelled_count += 1;
            }
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32
            ) {
                saw_rx_timeout = true;
            }
            false // never match -- just drain and observe for the whole window
        })
        .await
    );

    assert_eq!(
        cancelled_count, 1,
        "exactly one PduCopstCancelled should be observed"
    );
    assert!(
        !saw_rx_timeout,
        "no PduErrEvtRxTimeout should follow a cancellation the client already has \
         PduCopstCancelled for (ADR-095 amendment's round-9 correction: the executing_cop \
         exclusion in reap_cancelled_detached_registrants)"
    );

    drop(events);
    server.shutdown().await;
}

/// D-PDU `CP_SuspendQueueOnError` ComParam id (ADR-147). Not exported by the
/// crate, so duplicated here as a literal for the test, mirroring
/// `queue_error_suspend.rs`'s own local copy of the same constant.
const CP_SUSPEND_QUEUE_ON_ERROR: u32 = 0x802A;

/// ADR-182: a finite-`N` (`NumReceiveCycles > 0`) created-receive-only COP
/// with the default `CP_CyclicRespTimeout = 0` (disabled) on a quiet bus
/// (no matches ever arrive) stays `PDU_COPST_EXECUTING` well past where the
/// pre-ADR-182 `CP_P2Max`-derived inline window would have fired a spurious
/// `PduErrEvtRxTimeout` -- it now behaves like the `-1` subtype's own
/// disabled-timeout case (`receive_only_cyclic_disabled_timeout_never_finishes_on_its_own`)
/// instead of the old `CP_P2Max`-governed inline wait. `CancelComPrimitive`
/// still cleanly cancels it (the same detached-cancel mechanism the `-1`
/// subtype already uses).
#[tokio::test]
#[serial]
async fn receive_only_finite_n_default_timeout_stays_executing_past_old_p2max_window_then_cancels_cleanly()
 {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // A short CP_P2Max -- under pre-ADR-182 behavior this finite-N COP would
    // have timed out well inside the window this test waits.
    // CP_CyclicRespTimeout is left unset (defaults to 0/disabled).
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 100_000), // 100 ms
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        3,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    let is_finished_for_cop = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_handle.cop_handle)
    };

    // Well past the old 100ms CP_P2Max window (and well past a handful of
    // POLL_INTERVAL_MS ticks) -- with CP_CyclicRespTimeout disabled, the COP
    // must still be alive, no error.
    let mut saw_rx_timeout = false;
    assert!(
        !wait_for_event(&mut events, 400, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32
            ) {
                saw_rx_timeout = true;
            }
            is_finished_for_cop(item)
        })
        .await,
        "a finite-N created-receive-only COP with CP_CyclicRespTimeout disabled must not finish \
         on its own, even well past the old CP_P2Max window"
    );
    assert!(
        !saw_rx_timeout,
        "no PduErrEvtRxTimeout should fire for a quiet bus once CP_P2Max no longer governs this \
         COP (ADR-182)"
    );
    assert_eq!(
        cop_status(&mut client, cop_handle).await,
        PduComPrimitiveStatus::PduCopstExecuting,
        "the detached registrant should still report Executing"
    );

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "CancelComPrimitive should still cleanly cancel the detached, un-deadlined registrant"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-182: a finite-`N` created-receive-only COP with a nonzero
/// `CP_CyclicRespTimeout` and no matches ever arriving expires with
/// `PduErrEvtRxTimeout` at approximately the configured timeout, THEN
/// transitions `PDU_COPST_FINISHED` -- unlike the `-1` subtype (unaffected,
/// FINISHED-only), a finite-N registrant's target count was never reached,
/// so per ISO 22900-2:2022 Table 6's RECEIVE ONLY row this expiry is the
/// family's error path. The ADR-147 `CP_SuspendQueueOnError` hook must
/// actually fire -- verified via `timeout_suspends_queue_until_explicit_resume`'s
/// own pattern (`queue_error_suspend.rs`): a subsequently-queued
/// transmitting COP is held until an explicit `PDU_IOCTL_RESUME_TX_QUEUE`.
#[tokio::test]
#[serial]
async fn receive_only_finite_n_cyclic_timeout_errors_then_finishes_with_suspend_queue_hook() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CYCLIC_RESP_TIMEOUT, 200_000), // 200 ms
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        2,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    let is_finished_for_cop = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_handle.cop_handle)
    };

    // Well before the 200 ms deadline, the COP is still alive.
    assert!(
        !wait_for_event(&mut events, 60, is_finished_for_cop).await,
        "the cyclic timeout must not fire before CP_CyclicRespTimeout elapses"
    );

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
            is_finished_for_cop(item)
        })
        .await,
        "CP_CyclicRespTimeout should finish the COP once it elapses with the target count unmet"
    );
    assert!(
        saw_rx_timeout,
        "a finite-N registrant's CP_CyclicRespTimeout expiry with the count unmet should raise \
         PduErrEvtRxTimeout before finishing (ADR-182)"
    );

    // The timeout above must have suspended the queue (CP_SuspendQueueOnError
    // = 1): a fresh transmitting COP queued now must be held, not written.
    start_send_recv(&mut client, cll_handle, vec![0xAA], 0, 1, 0, vec![])
        .await
        .expect("start_com_primitive should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "a COP queued after the finite-N cyclic timeout must stay held while \
         CP_SuspendQueueOnError = 1 -- confirms the ADR-147 hook actually fired"
    );

    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    drop(events);
    server.shutdown().await;
}

/// Edge-case-hunter regression (ADR-182 follow-up fix), general end-to-end
/// coverage: `CancelComPrimitive` racing a finite-`N` created-receive-only
/// COP's active `CP_CyclicRespTimeout` deadline must still win -- the
/// observed terminal status must be `PduCopstCancelled`, never
/// `PduCopstFinished`, with no spurious `PduErrEvtRxTimeout` and no
/// lingering `CP_SuspendQueueOnError` TX-queue suspension.
///
/// Before the fix, `reap_expired_cyclic_registrants` (S6, ADR-100 Decision
/// §4/S6) never consulted `link.cancelled_cops`: `run_due_tick_duties` calls
/// `reap_cancelled_detached_registrants` (S5) immediately before S6 every
/// tick, so a `CancelComPrimitive` landing after S5's own scan but before
/// S6's, in the SAME tick, was invisible to S6 -- which would then wrongly
/// apply the ADR-182 finite-N expiry sequence (`PduErrEvtRxTimeout` +
/// `CP_SuspendQueueOnError` + `PduCopstFinished`) to a registrant the client
/// had already asked to cancel.
///
/// IMPORTANT SCOPE NOTE: this test does NOT prove it lands inside that
/// specific same-tick S5-vs-S6 gap, and is not this fix's primary
/// regression coverage. No deterministic tick-interleaving hook exists in
/// this harness (checked: `POLL_INTERVAL_MS` -- `events.rs` -- is a fixed
/// 10ms compile-time constant with no test-only override), and this fix's
/// own verification empirically found the gap itself is narrower than any
/// realistic external gRPC round trip's own jitter in this environment:
/// neither a single well-timed `CancelComPrimitive` (a fixed margin before
/// the deadline, mirroring the margin sweep this file's own
/// `tier1_wait_recheck_survives_many_deadline_vs_companion_watermark_races_in_one_wait`
/// uses) nor a burst of many concurrent ones (this test's own approach,
/// below) showed any measurable difference in outcome between this fix
/// present vs. reverted, across a wide sweep of `CP_CyclicRespTimeout`
/// values -- a single-shot margin only ever produced "genuinely too late"
/// (deadline already reaped, uncancelled, before the RPC could land -- a
/// real lost race, correctly Finished, not this bug) or "safely early" (S5
/// -- `reap_cancelled_detached_registrants` -- catches it on an earlier
/// tick, well before the deadline is even relevant), never the gap itself.
/// `events_reap_expired_cyclic_decision_tests.rs` (`events.rs`'s own test
/// module, next to `is_cyclic_reap_sound_tests`) is what actually pins the
/// fix's decision logic deterministically, via the pure `reap_expired_
/// cyclic_decision` extraction -- confirmed fail-without/pass-with there.
///
/// This test instead provides defense-in-depth end-to-end coverage of the
/// general mechanism (a cancel landing anywhere from well before the
/// deadline through shortly after it must still resolve to Cancelled, with
/// no error/suspend side effect): a short-but-not-razor-thin
/// `CP_CyclicRespTimeout`, with `CONCURRENT_CANCELS` `CancelComPrimitive`
/// calls fired at once (not sequentially) the instant the COP handle is
/// known, repeated across many independent rounds. Calibrated (this
/// sandbox) to pass reliably rather than to chase the gap itself, since a
/// design that only sometimes wins that race would make this test flaky for
/// a reason unrelated to catching a real regression -- see the scope note
/// above for why that tighter goal is left to the deterministic unit tests
/// instead.
///
/// ADR-149 follow-up (edge-case-hunter re-verification): the original 30ms
/// `CP_CyclicRespTimeout` was well under `GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS`
/// (harness.rs, ~85ms measured), so under real CPU contention (confirmed at
/// 16-way parallelism on 4 cores, ~37.5% failure rate driving the compiled
/// test binary directly) the `CONCURRENT_CANCELS` burst could genuinely lose
/// to the deadline on round-trip latency alone -- a false negative unrelated
/// to the ADR-182 same-tick race this test targets, not a real regression
/// (the deterministic unit tests above independently confirm the fix logic).
/// Widened to 300ms (~3.5x the ceiling, in this file's usual "generous
/// headroom" range for margin fixes -- see `harness.rs`'s module doc) so the
/// burst has real headroom against scheduling jitter while still being far
/// below the 2s per-round `wait_for_event` timeout and short enough that a
/// missing/broken fix would still show up promptly.
#[tokio::test]
#[serial]
async fn receive_only_finite_n_cancel_races_cyclic_timeout_expiry_stays_cancelled() {
    const ROUNDS: usize = 15;
    const CONCURRENT_CANCELS: usize = 40;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CYCLIC_RESP_TIMEOUT, 300_000), // 300ms (~3.5x GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS) -- see doc comment.
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    for round in 0..ROUNDS {
        let cop_handle = start_send_recv(
            &mut client,
            cll_handle,
            vec![0x22, 0xF1, 0x90],
            0,
            0,
            2, // finite N=2, never matched -- only ever finishes via count or expiry.
            vec![expect_positive_response(9)],
        )
        .await
        .expect("start_com_primitive should succeed");

        // Fire CONCURRENT_CANCELS CancelComPrimitive calls at once, over
        // independent client connections sharing the same underlying
        // channel -- see doc comment for why concurrent, not a single timed
        // call.
        let mut tasks = Vec::with_capacity(CONCURRENT_CANCELS);
        for _ in 0..CONCURRENT_CANCELS {
            let mut c = client.clone();
            tasks.push(tokio::spawn(async move {
                // Errors (e.g. a handle already reaped by a sibling
                // concurrent call) are expected and fine here -- only one
                // of these needs to land to record the cancellation; see
                // `sendrecv_cancel_on_already_finished_cop_returns_success_not_invalid_handle`
                // for why cancelling an already-terminal handle is itself a
                // harmless no-op, not an error, in the common case.
                let _ = c
                    .cancel_com_primitive(CancelComPrimitiveRequest {
                        cop_handle: Some(cop_handle),
                    })
                    .await;
            }));
        }
        for task in tasks {
            task.await.expect("cancel task should not panic");
        }

        let mut saw_rx_timeout_for_this_cop = false;
        let mut saw_finished_for_this_cop = false;
        assert!(
            wait_for_event(&mut events, 2000, |item| {
                if item
                    .cop_handle
                    .as_ref()
                    .is_none_or(|h| h.cop_handle != cop_handle.cop_handle)
                {
                    return false;
                }
                if matches!(
                    &item.data,
                    Some(event_item::Data::ErrorData(error))
                        if *error == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32
                ) {
                    saw_rx_timeout_for_this_cop = true;
                }
                if is_finished(item) {
                    saw_finished_for_this_cop = true;
                }
                is_cancelled(item) || is_finished(item)
            })
            .await,
            "round {round}: cop {cop_handle:?} should reach a terminal status within 2s"
        );

        assert!(
            !saw_finished_for_this_cop,
            "round {round}: CancelComPrimitive racing the CP_CyclicRespTimeout deadline must \
             still win -- got PduCopstFinished instead of PduCopstCancelled \
             (reap_expired_cyclic_registrants must skip a cop_handle already in \
             link.cancelled_cops)"
        );
        assert!(
            !saw_rx_timeout_for_this_cop,
            "round {round}: no PduErrEvtRxTimeout should fire for a COP the client already \
             cancelled"
        );
    }

    // Across all ROUNDS rounds, the ADR-147 CP_SuspendQueueOnError hook must
    // never have fired: a fresh transmitting COP queued now must be written
    // promptly, with no PDU_IOCTL_RESUME_TX_QUEUE needed.
    start_send_recv(&mut client, cll_handle, vec![0xAA], 0, 1, 0, vec![])
        .await
        .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    drop(events);
    server.shutdown().await;
}

/// ADR-182: "deadline restarted per accepted match" applies to the widened
/// finite-N family too (mirrors the `-1` subtype's own
/// `receive_only_cyclic_deadline_restarts_on_each_match_then_times_out_when_they_stop`),
/// but a finite-N registrant additionally has a target count -- reaching it
/// wins over the cyclic deadline entirely: the COP transitions
/// `PDU_COPST_FINISHED` on the final accepted match with no
/// `PduErrEvtRxTimeout`, well before its own (repeatedly restarted)
/// deadline would ever expire.
#[tokio::test]
#[serial]
async fn receive_only_finite_n_cyclic_deadline_restarts_on_match_then_count_completion_wins() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CYCLIC_RESP_TIMEOUT, 200_000), // 200 ms
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        3,
        vec![expect_positive_response(3)],
    )
    .await
    .expect("start_com_primitive should succeed");
    let is_finished_for_cop = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_handle.cop_handle)
    };

    tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;

    let mut saw_rx_timeout = false;
    // Three matches, ~130 ms apart -- each well inside the 200 ms window, so
    // every one restarts the deadline. By the third (final) match, well over
    // 200 ms has elapsed since the FIRST match (the original, un-restarted
    // deadline would have fired long before this point) -- but the third
    // match also reaches the target count, so completion should win.
    for (i, byte) in [0x01u8, 0x02u8, 0x03u8].into_iter().enumerate() {
        server.backdoor.inject_rx(
            MOCK_CHANNEL_ID,
            &can_frame(0x7E8, &[0x62, 0xF1, 0x90, byte]),
            j2534_0404::ISO15765,
        );
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
                    &item.data,
                    Some(event_item::Data::ResultData(result)) if result.acceptance_id == 3
                )
            })
            .await,
            "each injected frame should be accepted as a match"
        );
        if i < 2 {
            tokio::time::sleep(tokio::time::Duration::from_millis(130)).await;
        }
    }

    assert!(
        wait_for_event(&mut events, 300, is_finished_for_cop).await,
        "reaching the target count should finish the COP promptly, without waiting out the \
         (repeatedly restarted) 200ms cyclic deadline"
    );
    assert!(
        !saw_rx_timeout,
        "count-completion must win over the cyclic deadline -- no PduErrEvtRxTimeout should ever \
         be observed"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-182: count-completion FINISHED has at most one `POLL_INTERVAL_MS`
/// (10ms) maintenance-tick's worth of latency after the completing match is
/// accepted -- it is reap-driven (`reap_expired_cyclic_registrants`), not
/// immediate. Asserts an upper bound, not exact-zero latency, generously
/// above `GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS` (harness.rs) to avoid a
/// timing-flake false negative while still being far below the multi-second
/// scale a regression (e.g. a missing count-completion reap condition
/// entirely) would produce.
#[tokio::test]
#[serial]
async fn receive_only_finite_n_count_completion_latency_bounded_by_poll_interval() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // CP_CyclicRespTimeout left unset (disabled) -- isolates the
    // count-completion reap path from any deadline-expiry interaction.
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        1,
        vec![expect_positive_response(9)],
    )
    .await
    .expect("start_com_primitive should succeed");
    let is_finished_for_cop = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_handle.cop_handle)
    };

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 9
        ))
        .await,
        "the single required match should be accepted"
    );
    let matched_at = tokio::time::Instant::now();

    assert!(
        wait_for_event(&mut events, 2000, is_finished_for_cop).await,
        "the COP should finish once the target count is reached"
    );
    let latency = matched_at.elapsed();
    assert!(
        latency < tokio::time::Duration::from_millis(400),
        "count-completion FINISHED should follow the completing match within roughly one \
         maintenance tick, not {latency:?}"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-182 Do-NOT-do item: `NumReceiveCycles == -2` (IS-MULTIPLE)
/// created-receive-only is completely unaffected by the widened detach gate
/// -- it stays tier-2-from-creation but keeps running inline/blocking,
/// `CP_P2Max`-governed, exactly as before ADR-182. Also closes the P3 backlog
/// item (`implementation-notes.md`, "edge-case-hunter review of the round-9
/// fix above") noting the `-2` created-receive-only shape had no dedicated
/// regression test of its own: same assertion shape as
/// `receive_only_finite_n_created_cop_is_tier_two_registration_order_precedence`,
/// substituting `-2` for the finite-N COP.
#[tokio::test]
#[serial]
async fn receive_only_is_multiple_created_cop_is_tier_two_registration_order_precedence_unaffected_by_adr_182()
 {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 2_000_000),
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
    let mut events = subscribe(&mut client, cll_handle).await;

    // Older tier-2 monitor: created-receive-only, NumReceiveCycles = -1, a
    // vacuous (empty mask/pattern) descriptor that claims any frame.
    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![ExpectedResponseData {
            response_type: 0,
            acceptance_id: 100,
            mask_data: vec![],
            pattern_data: vec![],
            unique_resp_ids: vec![],
        }],
    )
    .await
    .expect("start_com_primitive should succeed");

    // Newer IS-MULTIPLE (-2) created-receive-only COP with a SPECIFIC
    // descriptor -- unaffected by ADR-182, stays tier-2 (ReceiveOnly) from
    // creation but running inline/blocking.
    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -2,
        vec![expect_positive_response(200)],
    )
    .await
    .expect("start_com_primitive should succeed");

    // Give the poll task time to register both (structural FIFO dispatch
    // order on this one physical channel's TX queue guarantees the OLDER
    // monitor above is registered first; NumSendCycles = 0 skips
    // transmission for both, so there is no `wait_for_written_count` signal
    // to block on instead).
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    // Matches BOTH the older monitor's vacuous descriptor and the newer
    // COP's specific one.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );

    let mut got: Option<u32> = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data {
                got = Some(result.acceptance_id);
                return true;
            }
            false
        })
        .await,
        "the frame should be delivered to one of the two registrants"
    );
    assert_eq!(
        got,
        Some(100),
        "the OLDER tier-2 monitor (registered first) should claim this frame -- the newer -2 \
         created-receive-only COP is tier-2 from creation (unaffected by ADR-182) and so must \
         still lose registration-order precedence within tier 2"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-100 round-9 Finding-2 correction / ADR-101 Decision §A's new `tier`
/// merge bullet: `bind_registrant` flips a true IS-CYCLIC COP's `tier` to
/// `ReceiveOnly` INLINE, immediately after accepting its first match, so
/// every LATER frame in the SAME `PassThruReadMsgs` batch is scanned against
/// the already-migrated tier -- not the stale tier-1 snapshot the outer,
/// post-batch `migrate_registrant_to_receive_only` call would otherwise
/// leave in place for the rest of that batch.
///
/// Two frames are queued in one synchronous burst (landing in the same
/// `MAX_POLL_MESSAGES`-bounded batch, structural ordering per this suite's
/// established discipline, not timing-lucky): the first is this IS-CYCLIC
/// COP's own first (genuine) match; the second, later in the SAME batch,
/// also matches this COP's descriptor, but should now lose registration-
/// order precedence to an OLDER tier-2 monitor with a vacuous descriptor,
/// once this COP has migrated to tier 2 itself.
///
/// Fail-without/pass-with proof: with the `bind_registrant` in-batch flip
/// (`if r.migrate_on_first_match && r.tier == ActiveSendReceive { r.tier =
/// ReceiveOnly; }`) reverted -- leaving only the outer, post-batch
/// `migrate_registrant_to_receive_only` call -- the IS-CYCLIC COP's
/// registrant stays `ActiveSendReceive` for the REST of this batch, so its
/// own non-vacuous descriptor wins step 2 of the precedence table again for
/// the second frame, stealing it from the older monitor; this test's second
/// assertion fails (`deliveries == vec![200, 200]`). Verified manually:
/// reverting just the in-batch flip reproduces the failure; restoring it
/// passes.
#[tokio::test]
#[serial]
async fn is_cyclic_migrates_tier_within_the_same_batch_not_just_the_next_pass() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 2_000_000),
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
    let mut events = subscribe(&mut client, cll_handle).await;

    // Older tier-2 monitor, registered first, vacuous descriptor.
    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        0,
        -1,
        vec![ExpectedResponseData {
            response_type: 0,
            acceptance_id: 100,
            mask_data: vec![],
            pattern_data: vec![],
            unique_resp_ids: vec![],
        }],
    )
    .await
    .expect("start_com_primitive should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    // True IS-CYCLIC COP: has a send phase (NumSendCycles = 1),
    // NumReceiveCycles = -1, a SPECIFIC descriptor.
    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        0,
        1,
        -1,
        vec![expect_positive_response(200)],
    )
    .await
    .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Both frames queued before the poll task's next PassThruReadMsgs call
    // -- one synchronous burst, no `.await` in between -- so they land in
    // the SAME batch (same discipline as the round-6/round-7 tests above).
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x02]),
        j2534_0404::ISO15765,
    );

    let mut deliveries: Vec<u32> = Vec::new();
    for _ in 0..2 {
        assert!(
            wait_for_event(&mut events, 2000, |item| {
                if let Some(event_item::Data::ResultData(result)) = &item.data {
                    deliveries.push(result.acceptance_id);
                    return true;
                }
                false
            })
            .await,
            "expected two ResultData deliveries from the single batch"
        );
    }

    assert_eq!(
        deliveries,
        vec![200, 100],
        "the first frame is this IS-CYCLIC COP's own genuine first match (200); the SECOND frame \
         in the SAME batch must already see it migrated to tier 2, losing registration-order \
         precedence to the OLDER tier-2 monitor (100) -- not stolen again by a stale tier-1 \
         snapshot"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-117: a freshly queued `CoptSendrecv` the poll task has not yet
/// dispatched reports `PduCopstIdle`, not `PduCopstWaiting` (ISO 22900-2
/// §D.1.4). A `CoptDelay` queued first structurally holds the poll task busy
/// so `cop_a` sits behind it, never yet dispatched, for a reliable
/// observation window (same technique `queue_delay` is used for elsewhere in
/// this file).
#[tokio::test]
#[serial]
async fn sendrecv_queued_never_dispatched_cop_reports_idle_not_waiting() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    queue_delay(&mut client, cll_handle, 300).await;

    let cop_a = start_send_recv(&mut client, cll_handle, vec![0x01, 0x02], 0, 1, 0, vec![])
        .await
        .expect("start_com_primitive should succeed (queued behind CoptDelay)");

    // The CoptDelay above is still occupying the poll task -- cop_a has never
    // been dispatched, so it must read Idle, not Waiting.
    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstIdle,
        "a queued-but-never-dispatched COP must report Idle, not Waiting"
    );

    // Let the delay finish and cop_a run to completion before shutdown.
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "expected the CoptDelay to finish"
    );
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "expected cop_a to finish once dispatched"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-117: a 2-cycle cyclic `CoptSendrecv` (`NumSendCycles = 2`,
/// `NumReceiveCycles = 1` per cycle) walks through
/// Idle -> Executing -> Waiting (resting between cycles) -> Executing ->
/// Finished, exercising the Idle/Waiting distinction (`CopEntry::dispatched`)
/// against a real cyclic COP rather than just the boundary cases above and
/// below.
///
/// ADR-118 (A2-24): also pins the exact `SubscribeEvent`-visible status
/// sequence for `cop_a` -- `Executing, Waiting, Executing, Finished` -- since
/// this GetStatus-polling test already walks through every transition this
/// finding is about.
#[tokio::test]
#[serial]
async fn sendrecv_two_cycle_cop_walks_idle_executing_waiting_executing_finished() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 3_000_000),
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
    let mut events = subscribe(&mut client, cll_handle).await;
    // ADR-118: accumulates every `CopStatus` event for cop_a seen on this
    // subscription, across every `wait_for_event` read below (its predicate
    // runs on every item, matching or not) -- nothing reads from `events`
    // before the first `wait_for_event` call further down, so this captures
    // the full sequence from cop_a's very first EXECUTING onward.
    let mut statuses: Vec<PduComPrimitiveStatus> = Vec::new();

    // Hold the poll task busy first so cop_a's pre-dispatch Idle window is
    // structurally guaranteed rather than timing-lucky.
    queue_delay(&mut client, cll_handle, 300).await;

    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        1000,
        2,
        1,
        vec![expect_positive_response(7)],
    )
    .await
    .expect("start_com_primitive should succeed");

    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstIdle,
        "cop_a is queued behind the CoptDelay, never yet dispatched"
    );

    // The CoptDelay finishes, then cop_a's first cycle dispatches and its
    // send reaches the adapter -- cop_a is now executing its first cycle's
    // receive phase (its CP_P2Max window is 3s, comfortably long enough to
    // observe Executing before we supply the match).
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstExecuting,
        "cop_a should be executing its first cycle's receive phase"
    );

    // First matching response completes cycle 1's receive phase; one more
    // cycle remains, so cop_a parks (cycle_time_ms = 1000 has not elapsed)
    // rather than finishing.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::CopStatus(status)) = &item.data
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
            {
                statuses.push(
                    PduComPrimitiveStatus::try_from(*status).unwrap_or_else(|_| {
                        panic!("unexpected PduComPrimitiveStatus value {status}")
                    }),
                );
            }
            matches!(
                &item.data,
                Some(event_item::Data::ResultData(result)) if result.acceptance_id == 7
            )
        })
        .await,
        "cop_a should accept its first cycle's response"
    );
    // A brief, harmless window remains between the ResultData event being
    // delivered and the poll task clearing `executing_cop` after
    // `handle_send_recv` returns (ADR-021's own accepted race window) --
    // poll rather than assert on the very first read.
    wait_for_cop_status(&mut client, cop_a, PduComPrimitiveStatus::PduCopstWaiting).await;

    // Second cycle dispatches once the 1000ms cycle time elapses.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstExecuting,
        "cop_a should be executing its second (and last) cycle's receive phase"
    );

    // Second matching response completes the COP (last cycle).
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x02]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::CopStatus(status)) = &item.data
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
            {
                statuses.push(
                    PduComPrimitiveStatus::try_from(*status).unwrap_or_else(|_| {
                        panic!("unexpected PduComPrimitiveStatus value {status}")
                    }),
                );
            }
            is_finished(item)
        })
        .await,
        "cop_a should finish after its second cycle's match"
    );

    // ADR-118 (A2-24): EXECUTING at each cycle's start, WAITING once while
    // parked between the two cycles, FINISHED once at the end -- no WAITING
    // before the final FINISHED (ISO 22900-2:2009(E) §9.4.17.2.2 d)).
    assert_eq!(
        statuses,
        vec![
            PduComPrimitiveStatus::PduCopstExecuting,
            PduComPrimitiveStatus::PduCopstWaiting,
            PduComPrimitiveStatus::PduCopstExecuting,
            PduComPrimitiveStatus::PduCopstFinished,
        ],
        "SubscribeEvent should see EXECUTING at each cycle start and WAITING between cycles, \
         not just via GetStatus polling"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-118 (A2-24): a single-cycle (non-repeating) `CoptSendrecv`
/// (`NumSendCycles = 1`) never parks between cycles -- there is no "between
/// cycles" for it -- so its `SubscribeEvent`-visible status sequence is
/// exactly `Executing, Finished`, with no `Waiting` in between (ISO
/// 22900-2:2009(E) §9.4.17.2.2 d): no WAITING event before the final
/// FINISHED transition).
#[tokio::test]
#[serial]
async fn sendrecv_single_cycle_cop_emits_executing_then_finished_no_waiting() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_a = start_send_recv(&mut client, cll_handle, vec![0x01, 0x02], 0, 1, 0, vec![])
        .await
        .expect("start_com_primitive should succeed");

    let mut statuses: Vec<PduComPrimitiveStatus> = Vec::new();
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            let is_cop_a = item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_a.cop_handle);
            if let Some(event_item::Data::CopStatus(status)) = &item.data
                && is_cop_a
            {
                statuses.push(
                    PduComPrimitiveStatus::try_from(*status).unwrap_or_else(|_| {
                        panic!("unexpected PduComPrimitiveStatus value {status}")
                    }),
                );
            }
            is_finished(item) && is_cop_a
        })
        .await,
        "cop_a's single cycle should finish"
    );

    assert_eq!(
        statuses,
        vec![
            PduComPrimitiveStatus::PduCopstExecuting,
            PduComPrimitiveStatus::PduCopstFinished,
        ],
        "a single-cycle (non-repeating) COP must never emit WAITING -- only a cyclic \
         follow-up cycle boundary does"
    );

    drop(events);
    server.shutdown().await;
}

/// P2 backlog follow-up (`docs/implementation-notes.md`): `send_cop_status`
/// now enqueues into the same per-CLL `CllQueueItem`/`GetEventItem` queue
/// `send_cll_status` already used, so a COP's status transitions are
/// pollable even with no `SubscribeEvent` subscriber ever attached --
/// previously (`send_cop_status` subscription-only) they were silently
/// dropped in that case. Deliberately never calls `subscribe()`: the whole
/// point is to exercise the queued path, which a live subscriber would
/// short-circuit (ADR-115 single-consumer rule -- see
/// `drain_cop_status_items_until`'s own doc comment).
#[tokio::test]
#[serial]
async fn sendrecv_cop_status_is_queued_for_get_event_item_with_no_live_subscriber() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // No SubscribeEvent call anywhere in this test.
    let cop_a = start_send_recv(&mut client, cll_handle, vec![0x01, 0x02], 0, 1, 0, vec![])
        .await
        .expect("start_com_primitive should succeed");

    let statuses = drain_cop_status_items_until(
        &mut client,
        cll_handle,
        cop_a.cop_handle,
        PduComPrimitiveStatus::PduCopstFinished,
    )
    .await;

    assert_eq!(
        statuses,
        vec![
            PduComPrimitiveStatus::PduCopstExecuting,
            PduComPrimitiveStatus::PduCopstFinished,
        ],
        "GetEventItem should surface cop_a's Executing then Finished transitions even though \
         no SubscribeEvent subscriber was ever attached"
    );

    server.shutdown().await;
}

/// ADR-118 (A2-24): a 3-cycle cyclic `CoptSendrecv` pins the FULL
/// `SubscribeEvent`-visible sequence -- `Executing, Waiting, Executing,
/// Waiting, Executing, Finished` -- covering the SECOND WAITING->EXECUTING
/// pair that `sendrecv_two_cycle_cop_walks_idle_executing_waiting_executing_finished`
/// (only 2 cycles, one WAITING->EXECUTING pair) cannot exercise. A
/// regression that, say, only emitted WAITING once total (e.g. an
/// accidentally-sticky guard) would pass the 2-cycle test's assertion but
/// fail this one.
#[tokio::test]
#[serial]
async fn sendrecv_three_cycle_cop_pins_full_waiting_executing_sequence() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 3_000_000),
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
    let mut events = subscribe(&mut client, cll_handle).await;
    // ADR-118: accumulates every `CopStatus` event for cop_a seen on this
    // subscription -- see the 2-cycle sibling test above for why reading
    // starts clean from cop_a's very first EXECUTING.
    let mut statuses: Vec<PduComPrimitiveStatus> = Vec::new();

    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        1000,
        3,
        1,
        vec![expect_positive_response(7)],
    )
    .await
    .expect("start_com_primitive should succeed");

    // Cycle 1: send, match, park.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::CopStatus(status)) = &item.data
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
            {
                statuses.push(
                    PduComPrimitiveStatus::try_from(*status).unwrap_or_else(|_| {
                        panic!("unexpected PduComPrimitiveStatus value {status}")
                    }),
                );
            }
            matches!(
                &item.data,
                Some(event_item::Data::ResultData(result)) if result.acceptance_id == 7
            )
        })
        .await,
        "cop_a should accept its first cycle's response"
    );
    wait_for_cop_status(&mut client, cop_a, PduComPrimitiveStatus::PduCopstWaiting).await;

    // Cycle 2: send, match, park again.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x02]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::CopStatus(status)) = &item.data
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
            {
                statuses.push(
                    PduComPrimitiveStatus::try_from(*status).unwrap_or_else(|_| {
                        panic!("unexpected PduComPrimitiveStatus value {status}")
                    }),
                );
            }
            matches!(
                &item.data,
                Some(event_item::Data::ResultData(result)) if result.acceptance_id == 7
            )
        })
        .await,
        "cop_a should accept its second cycle's response"
    );
    wait_for_cop_status(&mut client, cop_a, PduComPrimitiveStatus::PduCopstWaiting).await;

    // Cycle 3 (last): send, match, finish -- no third WAITING.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x03]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::CopStatus(status)) = &item.data
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
            {
                statuses.push(
                    PduComPrimitiveStatus::try_from(*status).unwrap_or_else(|_| {
                        panic!("unexpected PduComPrimitiveStatus value {status}")
                    }),
                );
            }
            is_finished(item)
        })
        .await,
        "cop_a should finish after its third and final cycle's match"
    );

    assert_eq!(
        statuses,
        vec![
            PduComPrimitiveStatus::PduCopstExecuting,
            PduComPrimitiveStatus::PduCopstWaiting,
            PduComPrimitiveStatus::PduCopstExecuting,
            PduComPrimitiveStatus::PduCopstWaiting,
            PduComPrimitiveStatus::PduCopstExecuting,
            PduComPrimitiveStatus::PduCopstFinished,
        ],
        "a 3-cycle cyclic COP must emit EXECUTING/WAITING at BOTH cycle boundaries, not just \
         the first -- a regression specific to the second WAITING->EXECUTING pair would \
         otherwise go undetected by the 2-cycle test alone"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-117: a cyclic COP parked in `PduCopstWaiting` between cycles is
/// cancelled by `DisconnectComLogicalLink` (`cancel_link_cops`) rather than by
/// `dispatch_tx_item`'s own normal-completion tail. Confirms the
/// disconnect-cancel path still reports `Cancelled` afterward (not a stale
/// `Waiting`), and that a fresh COP started after reconnecting the same
/// `cll_handle` reads `Idle`, not `Waiting` -- `cancel_link_cops`'s
/// `primitives.remove` discards the cancelled COP's `CopEntry` (and its
/// `dispatched` flag) atomically, so there is nothing left to mislabel a
/// later COP on the same `cll_handle`.
///
/// ADR-118 (A2-24): also pins `cop_a`'s exact `SubscribeEvent`-visible status
/// sequence -- `Executing, Waiting, Cancelled` -- for the *non-racing* case:
/// this test waits for GetStatus to already report Waiting before
/// disconnecting, so cop_a's cycle-boundary WAITING has fully completed
/// before `cancel_link_cops` ever runs (not the accepted-residual race
/// window ADR-118 documents, where a disconnect's CANCELLED can land between
/// the WAITING emission's own `contains_key` check and its `send_cop_status`
/// call).
#[tokio::test]
#[serial]
async fn sendrecv_cyclic_cop_cancelled_by_disconnect_then_reconnect_starts_fresh_cop_idle() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 3_000_000),
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
    let mut events = subscribe(&mut client, cll_handle).await;
    // ADR-118: accumulates every `CopStatus` event for cop_a seen on this
    // subscription -- see the sibling two-cycle test above for why reading
    // starts clean from cop_a's very first EXECUTING.
    let mut statuses: Vec<PduComPrimitiveStatus> = Vec::new();

    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        1000,
        2,
        1,
        vec![expect_positive_response(7)],
    )
    .await
    .expect("start_com_primitive should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::CopStatus(status)) = &item.data
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
            {
                statuses.push(
                    PduComPrimitiveStatus::try_from(*status).unwrap_or_else(|_| {
                        panic!("unexpected PduComPrimitiveStatus value {status}")
                    }),
                );
            }
            matches!(
                &item.data,
                Some(event_item::Data::ResultData(result)) if result.acceptance_id == 7
            )
        })
        .await,
        "cop_a should accept its first cycle's response"
    );
    // cop_a is now resting between cycles -- its CopEntry::dispatched is true.
    wait_for_cop_status(&mut client, cop_a, PduComPrimitiveStatus::PduCopstWaiting).await;

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::CopStatus(status)) = &item.data
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
            {
                statuses.push(
                    PduComPrimitiveStatus::try_from(*status).unwrap_or_else(|_| {
                        panic!("unexpected PduComPrimitiveStatus value {status}")
                    }),
                );
            }
            is_cancelled(item)
        })
        .await,
        "cancel_link_cops should cancel cop_a's pending second cycle"
    );
    // ADR-128: `terminal_cops` reports the real recorded terminal status
    // (Cancelled), not the old unconditional Finished default.
    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstCancelled,
        "cop_a should report Cancelled (removed from primitives), not a stale Waiting"
    );
    assert_eq!(
        statuses,
        vec![
            PduComPrimitiveStatus::PduCopstExecuting,
            PduComPrimitiveStatus::PduCopstWaiting,
            PduComPrimitiveStatus::PduCopstCancelled,
        ],
        "cop_a should see EXECUTING for its first cycle, WAITING while parked, then \
         CANCELLED -- never a WAITING after CANCELLED"
    );

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("reconnecting cll_handle should succeed");

    // A fresh COP on the reconnected link must read Idle, never Waiting --
    // cop_a's CopEntry (and its dispatched flag) was discarded wholesale by
    // cancel_link_cops's primitives.remove above, so there is nothing left to
    // mislabel this new cop_handle.
    queue_delay(&mut client, cll_handle, 300).await;
    let cop_b = start_send_recv(&mut client, cll_handle, vec![0x01], 0, 1, 0, vec![])
        .await
        .expect("start_com_primitive should succeed (queued behind CoptDelay)");
    assert_eq!(
        cop_status(&mut client, cop_b).await,
        PduComPrimitiveStatus::PduCopstIdle,
        "a fresh COP after reconnect must read Idle, not a leaked Waiting"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-117: a cyclic COP's next cycle can be explicitly cancelled
/// (`CancelComPrimitive`) while parked in `PduCopstWaiting` between cycles --
/// `should_skip_cancelled_item` (not `cancel_link_cops`, since the CLL stays
/// connected) is what finalizes it. Confirms the cancel path still reports
/// `Cancelled` (not a stale `Waiting`), and that a fresh COP started afterward
/// on the same (still-connected) CLL reads `Idle`.
#[tokio::test]
#[serial]
async fn sendrecv_cyclic_cop_explicitly_cancelled_while_waiting_then_fresh_cop_reports_idle() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 3_000_000),
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        1000,
        2,
        1,
        vec![expect_positive_response(7)],
    )
    .await
    .expect("start_com_primitive should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            &item.data,
            Some(event_item::Data::ResultData(result)) if result.acceptance_id == 7
        ))
        .await,
        "cop_a should accept its first cycle's response"
    );
    // cop_a is now resting between cycles -- its CopEntry::dispatched is true.
    wait_for_cop_status(&mut client, cop_a, PduComPrimitiveStatus::PduCopstWaiting).await;

    // Explicitly cancel cop_a while it is parked, waiting for its second
    // cycle's 1000ms timer -- the poll task's next dequeue of this
    // continuation hits should_skip_cancelled_item's explicit-cancel branch,
    // not cancel_link_cops (the CLL is never disconnected here).
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("cancel_com_primitive on a parked cyclic cop should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "should_skip_cancelled_item should cancel cop_a's pending second cycle"
    );
    // ADR-128: `terminal_cops` reports the real recorded terminal status
    // (Cancelled), not the old unconditional Finished default.
    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstCancelled,
        "cop_a should report Cancelled (removed from primitives), not a stale Waiting"
    );

    // A fresh COP on the same, still-connected CLL must read Idle, never
    // Waiting -- cop_a's CopEntry (and its dispatched flag) was discarded by
    // should_skip_cancelled_item's primitives.remove above, so there is
    // nothing left to mislabel this new cop_handle.
    queue_delay(&mut client, cll_handle, 300).await;
    let cop_b = start_send_recv(&mut client, cll_handle, vec![0x01], 0, 1, 0, vec![])
        .await
        .expect("start_com_primitive should succeed (queued behind CoptDelay)");
    assert_eq!(
        cop_status(&mut client, cop_b).await,
        PduComPrimitiveStatus::PduCopstIdle,
        "a fresh COP after the cyclic cop's cancellation must read Idle, not a leaked Waiting"
    );

    drop(events);
    server.shutdown().await;
}

/// A2-24 edge case (ADR-118): unlike the sibling test above -- which cancels
/// only once cop_a is already resting in `PduCopstWaiting` between cycles --
/// this cancels while cop_a is still actively `Executing` its first cycle's
/// receive-phase wait, timed so the `CancelComPrimitive` lands concurrently
/// with the SAME response that completes that cycle's match quota.
///
/// This targets a narrower, structural gap: `ReceivePhaseOutcome::
/// CycleComplete`'s `Matched` arm (`wait_for_expected_response`'s inner
/// loop) returns the instant `num_receive_cycles` matches are collected,
/// bypassing that same loop's own bottom `cancelled_cops` check entirely for
/// this pass -- so a cancel landing anywhere before that point is only
/// caught by `handle_send_recv`'s own S4 post-receive-phase recheck.
/// `CancelComPrimitive` is awaited to completion *before* the
/// cycle-completing response is injected, so this is guaranteed by
/// construction rather than by winning a timing race: the RPC return sets
/// `cancelled_cops` before the response frame exists at all, so every poll
/// pass from that point on -- whether frameless (loop-bottom drain) or the
/// one that finally consumes the injected frame (S4's post-receive-phase
/// drain) -- observes the marker already set and lands squarely in the
/// window S4 must catch.
#[tokio::test]
#[serial]
async fn sendrecv_cancel_racing_the_cycle_completing_response_never_emits_waiting() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 3_000_000),
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
    let mut events = subscribe(&mut client, cll_handle).await;
    // ADR-118: accumulates every `CopStatus` event for cop_a seen on this
    // subscription -- see the two-cycle sequence test above for why reading
    // starts clean from cop_a's very first EXECUTING.
    let mut statuses: Vec<PduComPrimitiveStatus> = Vec::new();

    // NumSendCycles = 2 so a continuation cycle genuinely remains pending --
    // without a second cycle, `handle_send_recv` would never construct a
    // `CycleContinuation` at all and this race couldn't manifest.
    let cop_a = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x22, 0xF1, 0x90],
        1000,
        2,
        1,
        vec![expect_positive_response(7)],
    )
    .await
    .expect("start_com_primitive should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstExecuting,
        "cop_a should be executing its first cycle's receive phase before the race below"
    );

    // Await CancelComPrimitive to completion FIRST, then inject the response
    // that completes cycle 1's match quota -- see the doc comment above for
    // why this ordering makes cancelled_cops-before-Matched guaranteed by
    // construction: the response frame doesn't exist yet when the RPC
    // returns, so inject_rx (which runs synchronously here, with no
    // intervening .await) can only be observed by poll passes that already
    // see the marker set.
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_a),
        })
        .await
        .expect("cancel_com_primitive during the active receive-phase wait should succeed");
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );

    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::CopStatus(status)) = &item.data
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
            {
                statuses.push(
                    PduComPrimitiveStatus::try_from(*status).unwrap_or_else(|_| {
                        panic!("unexpected PduComPrimitiveStatus value {status}")
                    }),
                );
            }
            is_cancelled(item)
        })
        .await,
        "cop_a should be cancelled rather than parked for a continuation cycle it will never run"
    );
    // ADR-128: `terminal_cops` reports the real recorded terminal status
    // (Cancelled), not the old unconditional Finished default.
    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstCancelled,
        "cop_a should report Cancelled (removed from primitives) once cancellation settles, not \
         a leaked Waiting"
    );

    // The regression this test guards: a CancelComPrimitive racing the
    // cycle-completing response must never be followed by a WAITING for the
    // continuation cycle it just cancelled -- the client asked to cancel and
    // must see EXECUTING -> CANCELLED only.
    assert_eq!(
        statuses,
        vec![
            PduComPrimitiveStatus::PduCopstExecuting,
            PduComPrimitiveStatus::PduCopstCancelled,
        ],
        "a CancelComPrimitive racing the cycle-completing response must never produce a \
         spurious WAITING for the continuation cycle it cancelled (A2-24 S4 recheck gap)"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-117: a COP siphoned into `tx_held` by `PDU_IOCTL_SUSPEND_TX_QUEUE`
/// before the poll task ever dispatches it must still report Idle, not
/// Waiting -- `CopEntry::dispatched` is only set by `dispatch_tx_item`'s
/// actual dispatch path, which the TX-suspend siphon returns from early
/// (before setting `executing_cop`, per ADR-021's own comment), so a held
/// item is never marked dispatched either.
#[tokio::test]
#[serial]
async fn sendrecv_held_by_suspend_tx_queue_reports_idle_not_waiting() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;

    io_ctl_cll(&mut client, cll_handle, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    let cop_a = start_send_recv(&mut client, cll_handle, vec![0x01], 0, 1, 0, vec![])
        .await
        .expect("start_com_primitive should succeed (siphoned into tx_held)");

    // Give the poll task ample time to dequeue and park cop_a in tx_held --
    // it must never reach the mock while suspended.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "cop_a must not be written while the TX queue is suspended"
    );
    assert_eq!(
        cop_status(&mut client, cop_a).await,
        PduComPrimitiveStatus::PduCopstIdle,
        "a COP held in tx_held before ever being dispatched must report Idle, not Waiting"
    );

    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.shutdown().await;
}

/// A2-23 (ISO 22900-2:2009(E) §9.4.18.2 d) / §9.2.6.6, ADR-128):
/// `CancelComPrimitive` on a COP that already reached `PduCopstFinished` --
/// while its owning CLL is still alive -- is a no-op success, not
/// `PDU_ERR_INVALID_HANDLE`. `NumSendCycles = 1, NumReceiveCycles = 0`
/// finishes the COP right after its one send (no response wait needed),
/// mirroring `sendrecv_num_send_cycles_sends_that_many_times`'s
/// cycle-counting COP but with count 1. Before the fix, this Cancel call
/// returned a `tonic::Status` with `code() == tonic::Code::NotFound`
/// (`unknown_handle_status`'s code, per `error.rs`) because the COP had
/// already been removed from `primitives` by the time Cancel ran; the fix's
/// `terminal_cops` ledger now makes it succeed instead. `GetStatus`
/// afterwards must still report the real recorded terminal status
/// (`PduCopstFinished`), exercising `rpc_get_status`'s new `terminal_cops`
/// fallback rather than an unconditional assumption.
#[tokio::test]
#[serial]
async fn sendrecv_cancel_on_already_finished_cop_returns_success_not_invalid_handle() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(&mut client, cll_handle, vec![0x01, 0x02], 0, 1, 0, vec![])
        .await
        .expect("start_com_primitive should succeed");

    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "the COP should finish after its one send cycle"
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    // The actual A2-23 regression: cancelling an already-finished COP whose
    // CLL is still alive must succeed as a no-op, not fail with
    // PDU_ERR_INVALID_HANDLE.
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive on an already-finished COP should succeed as a no-op");

    // GetStatus still reports the real recorded terminal status (Finished),
    // not merely "no longer erroring" -- the terminal_cops fallback path in
    // rpc_get_status.
    assert_eq!(
        cop_status(&mut client, cop_handle).await,
        PduComPrimitiveStatus::PduCopstFinished,
        "GetStatus should keep reporting the recorded terminal status via terminal_cops"
    );

    drop(events);
    server.shutdown().await;
}

/// A2-23 regression guard (ADR-128): `CancelComPrimitive` on a `cop_handle`
/// that was never returned by `StartComPrimitive` -- not merely a finished
/// one -- must still fail with `PDU_ERR_INVALID_HANDLE`
/// (`unknown_handle_status`'s `tonic::Code::NotFound`). This is the guard
/// against an overly broad fix that would make CancelComPrimitive always
/// succeed: a cop_handle absent from BOTH `primitives` and `terminal_cops`
/// is a genuine invalid handle.
#[tokio::test]
#[serial]
async fn sendrecv_cancel_on_never_issued_cop_handle_still_returns_invalid_handle() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(&mut client, cll_handle, vec![0x01, 0x02], 0, 1, 0, vec![])
        .await
        .expect("start_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "the COP should finish after its one send cycle"
    );

    // A sentinel offset guaranteed never allocated by this test's own
    // sequential cop_handle allocator.
    let never_issued = ComPrimitiveHandle {
        cop_handle: cop_handle.cop_handle + 1_000_000,
        ..cop_handle
    };
    let status = client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(never_issued),
        })
        .await
        .expect_err("cancelling a never-issued cop_handle should still fail");
    assert_eq!(status.code(), tonic::Code::NotFound);

    drop(events);
    server.shutdown().await;
}

/// A2-23 (ADR-128) per-CLL purge: once the owning CLL is destroyed, its
/// `terminal_cops` ledger entries go with it (`rpc_destroy_com_logical_link`
/// purges every entry belonging to the destroyed CLL right after
/// `cancel_link_cops` runs). So a second `CancelComPrimitive` on the SAME
/// cop_handle used in `sendrecv_cancel_on_already_finished_cop_returns_
/// success_not_invalid_handle` above -- which succeeded once while the CLL
/// was alive -- must go back to failing with `PDU_ERR_INVALID_HANDLE` once
/// the CLL is gone: this is now a genuine miss on both maps, not a
/// terminal-status no-op.
#[tokio::test]
#[serial]
async fn sendrecv_cancel_on_finished_cop_fails_again_after_owning_cll_destroyed() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop_handle = start_send_recv(&mut client, cll_handle, vec![0x01, 0x02], 0, 1, 0, vec![])
        .await
        .expect("start_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "the COP should finish after its one send cycle"
    );

    // Confirm the no-op success once, while the CLL is still alive (same
    // assertion as the test above).
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive on an already-finished COP should succeed as a no-op");

    // Close the event stream before teardown: DisconnectComLogicalLink and
    // DestroyComLogicalLink both need the subscription's in-flight requests
    // to settle, and an open stream never ends on its own.
    drop(events);

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

    // The ledger entry was purged along with the CLL -- this is now a
    // genuine miss on both `primitives` and `terminal_cops`.
    let status = client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect_err(
            "cancelling the same cop_handle again after its owning CLL was destroyed should fail",
        );
    assert_eq!(status.code(), tonic::Code::NotFound);

    server.shutdown().await;
}

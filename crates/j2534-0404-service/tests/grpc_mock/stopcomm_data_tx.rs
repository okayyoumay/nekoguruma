//! ADR-085: `CoptStopcomm` transmits non-empty `cop_data` as a final
//! fire-and-forget message on the bus (fixing a prior data-loss bug where it
//! was silently discarded), resolved through the same `resolve_send_recv_tx`
//! pipeline `CoptSendrecv` uses, always against the Active ComParam snapshot.
//! Empty `cop_data` remains byte-for-byte the pre-ADR-085 behavior: no
//! transmit, and exempt from the physical TX-queue lock check.

use serial_test::serial;
use vci_service_interface::{
    CancelComPrimitiveRequest, ComLogicalLinkHandle, ComPrimitiveCtrlData,
    ConnectComLogicalLinkRequest, DisconnectComLogicalLinkRequest, ExpectedResponseData,
    GetObjectIdRequest, IoCtlRequest, LockResourceRequest, ObjectType, PduComPrimitiveStatus,
    PduError, PduErrorEvent, StartComPrimitiveRequest, UnlockResourceRequest,
    error_detail_from_status, event_item, io_ctl_request, subscribe_event_request,
    vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// `CP_P3Phys` ComParam ID (ADR-060), used to force a real, controllable
/// inter-request gap wait ahead of a StopComm final transmit -- same value
/// `p3_gap.rs` pins.
const CP_P3_PHYS: u32 = 0x80B4;

/// RC21 (NRC 0x21, BusyRepeatRequest) auto re-request ComParam IDs (ADR-018),
/// same values `rc_handling.rs` pins for its own (CoptSendrecv-only) NRC 0x78
/// coverage, generalized here to 0x21 to exercise the RC21 request-time sleep
/// path from a `CoptStopcomm` receive phase.
const CP_RC_BYTE_OFFSET: u32 = 0x8028;
const CP_RC21_HANDLING: u32 = 0x8021;
const CP_RC21_REQUEST_TIME: u32 = 0x8022;

/// RC78 (NRC 0x78, ResponsePending) auto-handling ComParam ID (ADR-018),
/// same value `rc_handling.rs` pins, used here (ADR-102) to exercise the
/// RC78 reload path from a `CoptStopcomm` IS-MULTIPLE receive phase.
const CP_RC78_HANDLING: u32 = 0x8027;

/// Resolves a `PDU_IOCTL_*` shortname via `GetObjectId(OBJT_IO_CTRL, ...)`,
/// matching `pdu_ioctl.rs`'s helper of the same name/shape.
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

/// Queues a `CoptDelay` of `time_ms` -- used to hold the FIFO poll-task
/// queue open across subsequent RPC calls so ordering is structural rather
/// than timing-lucky (same pattern `param_binding.rs`'s claim9/claim4 tests
/// use).
async fn queue_delay(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
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

/// How long [`hold_queue_on_sibling_cll`]'s `CoptDelay` keeps the shared
/// poll-task queue busy. It must cover the two or three gRPC round trips a
/// test makes while the hold is in place (ADR-149's per-round-trip margin),
/// and stay well inside the 2 s event waits that follow it.
const SIBLING_HOLD_MS: u32 = 500;

/// Connects a second CLL on the same physical channel as the test's own CLL
/// (so both share one poll task and its FIFO queue), for
/// [`hold_queue_on_sibling_cll`].
async fn connect_sibling_cll(
    server: &TestServer,
    client: &mut VciServiceClient<tonic::transport::Channel>,
) -> ComLogicalLinkHandle {
    let sibling =
        create_and_connect_cll(client, j2534_0404::CAN, &[(j2534_0404::DATA_RATE, 500_000)]).await;
    set_can_phys_req_id_and_promote(client, sibling, 0x7E1).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "the sibling CLL should share the test CLL's physical channel"
    );
    sibling
}

/// Holds the shared poll-task queue busy for [`SIBLING_HOLD_MS`] with a
/// `CoptDelay` on `sibling` (from [`connect_sibling_cll`]), so a `CoptStopcomm`
/// queued next on the test's own CLL is guaranteed to still sit in the queue
/// while the test makes further calls. The delay must not be on the test's
/// own CLL: accepting a `CoptStopcomm` marks every other COP on that CLL
/// cancelled, including a running `CoptDelay`, which then ends within one
/// poll interval and lets the StopComm run before the test's next call
/// arrives. A COP on another CLL is not touched by that sweep.
async fn hold_queue_on_sibling_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    sibling: ComLogicalLinkHandle,
) {
    queue_delay(client, sibling, SIBLING_HOLD_MS).await;
}

/// Waits for the next `PduCopstFinished` on an already-open event stream.
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

/// Waits for the next `PduCllstOnline` on an already-open event stream.
async fn wait_for_cll_online(
    events: &mut tonic::Streaming<vci_service_interface::EventNotification>,
) {
    assert!(
        wait_for_event(events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "expected a PduCllstOnline event"
    );
}

async fn start_comm(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) -> vci_service_interface::ComPrimitiveHandle {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
}

/// Like `wait_for_cop_finished`, but qualified to `cop_handle` -- for callers
/// where an earlier, unrelated COP (e.g. `set_unique_resp_table_and_promote`'s
/// own `CoptUpdateparam`, started and finished before the subscriber
/// attached) has its own backlogged `PduCopstFinished` sitting ahead of the
/// COP under test in the same live-flush FIFO (P2 backlog follow-up,
/// `docs/implementation-notes.md`), which an unqualified wait would match
/// first.
async fn wait_for_cop_finished_matching(
    events: &mut tonic::Streaming<vci_service_interface::EventNotification>,
    cop_handle: vci_service_interface::ComPrimitiveHandle,
) {
    assert!(
        wait_for_event(events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cop_handle.cop_handle))
        .await,
        "expected a PduCopstFinished event for cop_handle {cop_handle:?}"
    );
}

/// Empty `cop_data` on `CoptStopcomm` is byte-for-byte the pre-ADR-085
/// behavior: no `PassThruWriteMsgs` call, same `PduCllstOnline` /
/// `PduCopstFinished` event sequence as before.
#[tokio::test]
#[serial]
async fn stopcomm_empty_cop_data_does_not_transmit() {
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    let baseline_written = server.backdoor.written_count(MOCK_CHANNEL_ID);

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, empty cop_data) should succeed");
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        baseline_written,
        "empty cop_data must not transmit anything (pre-ADR-085 behavior unchanged)"
    );

    drop(events);
    server.shutdown().await;
}

/// Non-empty `cop_data` on `CoptStopcomm` transmits exactly one
/// `PassThruWriteMsgs`, with the resolved header-prefixed message, between
/// the periodic tester-present stop and `PduCllstOnline` (ADR-085). Since the
/// poll task processes `TxItem::StopComm` as a single, uninterrupted, FIFO
/// step, the write having landed by the time `PduCllstOnline`/
/// `PduCopstFinished` are observed demonstrates it happened in that window.
#[tokio::test]
#[serial]
async fn stopcomm_non_empty_cop_data_transmits_final_message() {
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    let final_payload = vec![0x82, 0xF1, 0x01]; // e.g. a KWP2000-shaped StopCommunication request
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload.clone(),
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, non-empty cop_data) should succeed");
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "non-empty cop_data must transmit exactly one message"
    );
    let mut expected = 0x7E0_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&final_payload);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    drop(events);
    server.shutdown().await;
}

/// `cop_data` that produces a message outside the protocol's SAE J2534-1 TX
/// size range (CAN: 4..=12 bytes, 4-byte header + up to 8 payload bytes) is
/// rejected synchronously (`INVALID_ARGUMENT`, ADR-049), reusing
/// `resolve_send_recv_tx`'s validation exactly as `CoptSendrecv` does -- the
/// COP is never enqueued, so `comm_started` stays `true` and no write occurs.
#[tokio::test]
#[serial]
async fn stopcomm_oversized_cop_data_rejected_synchronously() {
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    // 9-byte payload -> 4-byte header + 9 = 13 bytes, exceeds the 12-byte max.
    let too_long_payload = vec![0u8; 9];
    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: too_long_payload,
            cop_ctrl_data: None,
        })
        .await
        .expect_err("start_com_primitive(CoptStopcomm) should reject an oversized cop_data");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("4..=12"), "{}", status.message());
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    // The COP was never enqueued: comm is still started, so a follow-up
    // CoptStopcomm with valid (empty) cop_data succeeds normally.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "start_com_primitive(CoptStopcomm, empty cop_data) should succeed after the \
                 rejected oversized attempt",
        );
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// A `CoptStopcomm` carrying non-empty `cop_data` transmits like
/// `CoptSendrecv`/`CoptStartcomm`, so it is likewise subject to another
/// CLL's held `LOCK_PHYSICAL_TX_QUEUE` (ADR-085) -- but as of ADR-123 that is
/// no longer a synchronous reject: the COP is accepted and QUEUED (held in
/// `tx_held`) until the lock releases, then dispatched. Empty-data
/// `CoptStopcomm` stays exempt from the lock entirely, exactly as before
/// ADR-085, so teardown is never blocked by another CLL's TX-queue lock.
#[tokio::test]
#[serial]
async fn stopcomm_non_empty_cop_data_respects_physical_tx_queue_lock() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a and cll_b share one CAN/500kbps physical channel.
    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_b).await;
    wait_for_cop_finished(&mut events_b).await;

    // cll_a holds the physical TX queue lock on the channel cll_b shares.
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("lock_resource should succeed");

    // Non-empty cop_data: accepted (queued, not rejected) while cll_a holds
    // the TX-queue lock -- it must not transmit yet.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![0xAA],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "CoptStopcomm with non-empty cop_data should be queued, not rejected, while another \
             CLL holds the TX queue lock (ADR-123)",
        );
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the queued CoptStopcomm must not transmit while the lock is still held"
    );

    // Releasing the lock resumes dispatch: the queued final message
    // transmits and the COP finishes.
    client
        .unlock_resource(UnlockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("unlock_resource should succeed");
    wait_for_cll_online(&mut events_b).await;
    wait_for_cop_finished(&mut events_b).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the held CoptStopcomm must transmit once cll_a releases the TX queue lock"
    );

    drop(events_b);
    server.shutdown().await;
}

/// A synchronously-rejected `CoptStopcomm` (oversized `cop_data`, ADR-049)
/// must leave every OTHER queued ComPrimitive on the link untouched: since
/// the rejected `CoptStopcomm` never enqueues a `TxItem::StopComm`, it never
/// supersedes anything, so a sibling COP still queued behind it must reach
/// `PduCopstFinished` normally, not `PduCopstCancelled` (Codex-review fix --
/// an earlier version resolved `cop_data` validation AFTER already marking
/// every other queued COP on the link cancelled, so a rejected call still
/// cancelled COPs it never actually replaced).
#[tokio::test]
#[serial]
async fn stopcomm_rejected_oversized_cop_data_does_not_cancel_other_queued_cops() {
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    // Queue a short CoptDelay -- still sitting in the FIFO (or just starting
    // to execute) when the oversized CoptStopcomm below is rejected.
    let delay_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptDelay as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 200,
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

    // 9-byte payload -> 4-byte header + 9 = 13 bytes, exceeds the 12-byte max.
    let too_long_payload = vec![0u8; 9];
    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: too_long_payload,
            cop_ctrl_data: None,
        })
        .await
        .expect_err("start_com_primitive(CoptStopcomm) should reject an oversized cop_data");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    // The queued CoptDelay must still finish normally.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == delay_cop_handle))
        .await,
        "the CoptDelay queued before the rejected CoptStopcomm should reach PduCopstFinished, \
         not PduCopstCancelled -- the rejected CoptStopcomm never enqueued a TxItem::StopComm, \
         so it must not have cancelled anything"
    );

    // comm is still started: a follow-up empty-data CoptStopcomm succeeds.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, empty cop_data) should succeed");
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// ADR-085 amendment: a second concurrent `CoptStopcomm` on the same CLL,
/// issued while the first one is still queued/executing, is rejected
/// synchronously (`FAILED_PRECONDITION`, "already in progress") rather than
/// being accepted -- and the first StopComm's own final transmit and
/// terminal `PduCllstOnline`/`PduCopstFinished` sequence are unaffected by
/// the rejected second attempt.
///
/// A `CoptDelay` on a sibling CLL is queued ahead of the first
/// `CoptStopcomm` ([`hold_queue_on_sibling_cll`]) so that both the first
/// `CoptStopcomm`'s RPC call AND the second (racing) `CoptStopcomm` RPC call
/// are guaranteed to return while the first StopComm is still sitting
/// unprocessed in the poll task's FIFO queue -- i.e. genuinely "in
/// progress" per `stop_comm_pending`, not yet finished.
#[tokio::test]
#[serial]
async fn stopcomm_second_concurrent_call_rejected_while_first_in_progress() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let sibling = connect_sibling_cll(&server, &mut client).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    hold_queue_on_sibling_cll(&mut client, sibling).await;

    let final_payload = vec![0x82, 0xF1, 0x01];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload.clone(),
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, first call) should succeed");

    // Second, racing CoptStopcomm on the same CLL: rejected synchronously --
    // stop_comm_pending, set when the first call was accepted above, is
    // still true (comm_started alone would have let this through).
    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "a second concurrent CoptStopcomm should be rejected while the first is in progress",
        );
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status.message().contains("already in progress"),
        "{}",
        status.message()
    );
    // A2-28: this is a same-operation-already-running guard (nobody called
    // LockResource), so the closer code must be PDU_ERR_RESOURCE_BUSY, not
    // the lock-holder-conflict PDU_ERR_RSC_LOCKED.
    let error_detail =
        error_detail_from_status(&status).expect("failing RPC should carry an ErrorDetail");
    assert_eq!(error_detail.pdu_error, PduError::PduErrResourceBusy as i32);

    // The first StopComm's transmit and terminal sequence proceed
    // unaffected by the rejected second attempt.
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the first StopComm's non-empty cop_data must still transmit exactly once, unaffected \
         by the rejected second CoptStopcomm"
    );
    let mut expected = 0x7E0_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&final_payload);
    assert_eq!(server.backdoor.written_data(MOCK_CHANNEL_ID, 0), expected);

    // Exactly one PduCllstOnline: no duplicate from a second
    // handle_stop_comm run.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "must observe exactly one PduCllstOnline, not a duplicate from a second StopComm run"
    );

    drop(events);
    server.shutdown().await;
}

/// A full `CoptStartcomm` -> `CoptStopcomm` -> `CoptStartcomm` ->
/// `CoptStopcomm` cycle on the same CLL still works end-to-end, confirming
/// `stop_comm_pending` is correctly cleared at each StopComm's normal
/// completion (not left stuck `true`, which would permanently reject every
/// subsequent `CoptStopcomm` on this CLL).
#[tokio::test]
#[serial]
async fn stopcomm_pending_cleared_after_normal_completion_allows_full_cycle() {
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

    // First CoptStartcomm -> CoptStopcomm cycle.
    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, first cycle) should succeed");
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    // Second CoptStartcomm -> CoptStopcomm cycle: only reachable if the
    // first StopComm cleared both comm_started AND stop_comm_pending.
    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, second cycle) should succeed");
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// A synchronously-rejected `CoptStopcomm` (oversized `cop_data`, the same
/// scenario as `stopcomm_oversized_cop_data_rejected_synchronously`) must
/// roll back `stop_comm_pending` to `false` on its way out, not just leave
/// `comm_started` untouched: a follow-up valid `CoptStopcomm` issued
/// immediately afterward must succeed, not be rejected with "already in
/// progress" by a `stop_comm_pending` left stuck `true` from the rejected
/// attempt's test-and-set.
#[tokio::test]
#[serial]
async fn stopcomm_rejected_synchronously_rolls_back_stop_comm_pending() {
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    // 9-byte payload -> 4-byte header + 9 = 13 bytes, exceeds the 12-byte
    // max: rejected by resolve_send_recv_tx AFTER stop_comm_pending was
    // already set true by the test-and-set above it in the same RPC call.
    let too_long_payload = vec![0u8; 9];
    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: too_long_payload,
            cop_ctrl_data: None,
        })
        .await
        .expect_err("start_com_primitive(CoptStopcomm) should reject an oversized cop_data");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    // Immediately afterward: a valid CoptStopcomm must succeed. If the
    // rollback in the Err(err) arm did not clear stop_comm_pending, this
    // would fail with FAILED_PRECONDITION ("already in progress") instead.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "a valid CoptStopcomm immediately after a synchronously-rejected one should succeed \
             -- stop_comm_pending must have been rolled back",
        );
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// ADR-085 round-4 fix: a `CoptStopcomm` with non-empty `cop_data`, still
/// genuinely sitting in the poll task's mpsc queue (queued, not yet
/// dispatched, and never parked in `tx_held`) when `PDU_IOCTL_CLEAR_TX_QUEUE`
/// runs, must be fully cancelled: `PduCopstCancelled` (not `PduCopstFinished`)
/// for the StopComm's own `cop_handle`, its `cop_data` payload never
/// transmitted, and `stop_comm_pending` cleared so a follow-up `CoptStopcomm`
/// succeeds instead of being rejected "already in progress". A prior version
/// of `should_skip_cancelled_item` carved `StopComm` out of the skip path,
/// letting it execute (and, since this ADR, transmit) anyway even when
/// explicitly cancelled -- silently violating `CLEAR_TX_QUEUE`'s documented
/// "clear pending TX" contract.
///
/// A `CoptDelay` on a sibling CLL is queued ahead of the `CoptStopcomm`
/// ([`hold_queue_on_sibling_cll`]) so that the
/// `PDU_IOCTL_CLEAR_TX_QUEUE` call below is guaranteed to run while the
/// StopComm's `TxItem` is still sitting unprocessed in the poll task's mpsc
/// queue.
#[tokio::test]
#[serial]
async fn stopcomm_queued_and_cancelled_by_clear_tx_queue_never_transmits() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let sibling = connect_sibling_cll(&server, &mut client).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    hold_queue_on_sibling_cll(&mut client, sibling).await;

    let final_payload = vec![0x82, 0xF1, 0x01];
    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) should succeed (queued behind CoptDelay)")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;

    let clear_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_CLEAR_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, clear_id)
        .await
        .expect("PDU_IOCTL_CLEAR_TX_QUEUE should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle))
        .await,
        "the queued CoptStopcomm should be cancelled (PduCopstCancelled) by \
         PDU_IOCTL_CLEAR_TX_QUEUE, not executed to PduCopstFinished"
    );

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "a queued CoptStopcomm cancelled by PDU_IOCTL_CLEAR_TX_QUEUE must never transmit its \
         cop_data payload"
    );

    // comm_started stays true (the stop genuinely never happened): a
    // follow-up CoptStopcomm must succeed, not be rejected "already in
    // progress" -- proving stop_comm_pending was cleared in the same
    // critical section as the cancellation above.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "a follow-up CoptStopcomm after the queued-and-cancelled one should succeed -- \
             stop_comm_pending must have been cleared",
        );
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// Same scenario as `stopcomm_queued_and_cancelled_by_clear_tx_queue_never_transmits`,
/// but the cancellation source is an explicit `CancelComPrimitive` on the
/// queued StopComm's own `cop_handle` instead of `PDU_IOCTL_CLEAR_TX_QUEUE` --
/// both write the same `LogicalLinkState.cancelled_cops` set, so
/// `should_skip_cancelled_item` must treat them identically. Held open the
/// same way, with [`hold_queue_on_sibling_cll`].
#[tokio::test]
#[serial]
async fn stopcomm_queued_and_cancelled_by_cancel_com_primitive_never_transmits() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let sibling = connect_sibling_cll(&server, &mut client).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    hold_queue_on_sibling_cll(&mut client, sibling).await;

    let final_payload = vec![0x82, 0xF1, 0x01];
    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) should succeed (queued behind CoptDelay)")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(stop_cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle.cop_handle))
        .await,
        "the queued CoptStopcomm should be cancelled (PduCopstCancelled) by \
         CancelComPrimitive, not executed to PduCopstFinished"
    );

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "a queued CoptStopcomm cancelled by CancelComPrimitive must never transmit its cop_data \
         payload"
    );

    // comm_started stays true (the stop genuinely never happened): a
    // follow-up CoptStopcomm must succeed, not be rejected "already in
    // progress" -- proving stop_comm_pending was cleared in the same
    // critical section as the cancellation above.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "a follow-up CoptStopcomm after the queued-and-cancelled one should succeed -- \
             stop_comm_pending must have been cleared",
        );
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// A `CoptStopcomm` accepted while the CLL's TX queue is suspended
/// (`PDU_IOCTL_SUSPEND_TX_QUEUE`) is siphoned into `tx_held` before the poll
/// task ever runs `handle_stop_comm` -- the only other place that clears
/// `stop_comm_pending` on a non-error path. If `PDU_IOCTL_CLEAR_TX_QUEUE`
/// then drains and cancels that held item via `cancel_held_tx_items`, without
/// this fix `stop_comm_pending` would stay stuck `true` forever (the link is
/// still `comm_started`, so neither `handle_channel_hard_error` nor
/// disconnect ever runs to clear it either), permanently rejecting every
/// later `CoptStopcomm` on this CLL with "already in progress".
#[tokio::test]
#[serial]
async fn stopcomm_held_and_cancelled_by_clear_tx_queue_clears_stop_comm_pending() {
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let clear_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_CLEAR_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;

    io_ctl_cll(&mut client, cll_handle, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) should succeed (queued, then held)");

    // Give the poll task ample time to dequeue and park the item in
    // `tx_held` while the queue is suspended.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    io_ctl_cll(&mut client, cll_handle, clear_id)
        .await
        .expect("PDU_IOCTL_CLEAR_TX_QUEUE should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "the held CoptStopcomm should be cancelled by PDU_IOCTL_CLEAR_TX_QUEUE"
    );

    // PDU_IOCTL_CLEAR_TX_QUEUE deliberately leaves `tx_suspended` untouched
    // (it clears queued items, it does not resume dispatch) -- resume
    // explicitly so the next CoptStopcomm's TxItem is actually dispatched
    // rather than parked in `tx_held` again.
    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    // A later CoptStopcomm must be accepted -- comm_started is still true
    // (the cancelled StopComm never ran handle_stop_comm), and
    // stop_comm_pending must have been cleared by cancel_held_tx_items, not
    // left stuck from the cancelled attempt above.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "a CoptStopcomm after the held-and-cancelled one should succeed -- stop_comm_pending \
             must have been cleared by cancel_held_tx_items",
        );
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// A `CoptStopcomm` already parked in `tx_held` (by
/// `PDU_IOCTL_SUSPEND_TX_QUEUE`) that is cancelled via a bare
/// `CancelComPrimitive` -- with the queue never resumed, cleared, or
/// disconnected -- must be cancelled synchronously by the RPC itself
/// (ADR-085 round 5). Before this fix, nothing re-examined `tx_held` until a
/// later resume/clear/disconnect, so `stop_comm_pending` stayed stuck `true`
/// and a follow-up `CoptStopcomm` was rejected "already in progress" even
/// though `GetStatus` already reported the held COP Cancelled.
#[tokio::test]
#[serial]
async fn stopcomm_held_and_cancelled_by_cancel_com_primitive_clears_stop_comm_pending() {
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;

    io_ctl_cll(&mut client, cll_handle, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) should succeed (queued, then held)")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    // Give the poll task ample time to dequeue and park the item in
    // `tx_held` while the queue is suspended.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(stop_cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle.cop_handle))
        .await,
        "the held CoptStopcomm should be cancelled (PduCopstCancelled) by CancelComPrimitive, \
         not executed to PduCopstFinished"
    );

    // Without resuming, clearing, or disconnecting: a second CoptStopcomm
    // must be accepted immediately -- proving stop_comm_pending was cleared
    // synchronously by the cancel RPC itself, not by some later drain.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "a CoptStopcomm after the held-and-cancelled one should succeed without resuming -- \
             stop_comm_pending must have been cleared synchronously by CancelComPrimitive",
        );

    drop(events);
    server.shutdown().await;
}

/// ADR-085 round 6: `isotp_send`'s own FlowControl-wait cancellation check is
/// now gated by the same `cancellable: false` `handle_stop_comm`'s
/// `wait_for_p3_gap` call already used (round 1) -- a `CancelComPrimitive` on
/// a StopComm's own `cop_handle`, arriving mid-multi-frame transmit (after
/// the FirstFrame and some, but not all, ConsecutiveFrames have already gone
/// out), must not abort the remaining frames: per ISO 15765-2 the ECU
/// discards the whole message on an incomplete FirstFrame sequence (N_Cr
/// timeout), so a truncated send is "no delivery," not "partial delivery."
///
/// Uses a software-ISO-TP `SAE_ISO15765` CLL (`can_mode.rs`'s established
/// FlowControl-injection pattern) with `BlockSize = 1` on the first
/// FlowControl frame, forcing a SECOND FlowControl wait before the send
/// completes -- the cancel lands in that window, with the FirstFrame and the
/// first ConsecutiveFrame already on the wire, genuinely "mid-transmit,"
/// not merely queued or held.
#[tokio::test]
#[serial]
async fn stopcomm_multiframe_cancel_during_transmit_does_not_abort() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    // 20-byte payload -> FirstFrame (6 bytes) + 2 ConsecutiveFrames (7 bytes
    // each), the same segmentation `can_mode.rs`'s software-isotp tests pin.
    let payload: Vec<u8> = (1..=20).collect();
    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: payload.clone(),
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, non-empty cop_data) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    // FirstFrame goes out immediately.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // FlowControl #1: ContinueToSend, BlockSize = 1 (one ConsecutiveFrame per
    // block, forcing a SECOND FlowControl wait before the send completes),
    // STmin = 0.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x30, 0x01, 0x00]),
        j2534_0404::CAN,
    );

    // The first ConsecutiveFrame goes out, then isotp_send blocks waiting for
    // the second block's FlowControl -- genuinely mid-transmit.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    // CancelComPrimitive on the StopComm's own cop_handle while it is
    // blocked waiting for the second FlowControl frame: cancellable = false
    // at this call site (round 6), so this must NOT abort the remaining
    // ConsecutiveFrame.
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(stop_cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    // Give the poll task a moment to observe (and, per this fix, ignore) the
    // cancellation before unblocking it -- if it were honored, the send
    // would abort here with no further frames ever written.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "the second ConsecutiveFrame must not go out before its own FlowControl arrives"
    );

    // FlowControl #2 unblocks the final ConsecutiveFrame.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x30, 0x00, 0x00]),
        j2534_0404::CAN,
    );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let mut expected_ff = can_frame(0x7E0, &[0x10, 0x14]);
    expected_ff.extend_from_slice(&payload[..6]);
    let mut expected_cf1 = can_frame(0x7E0, &[0x21]);
    expected_cf1.extend_from_slice(&payload[6..13]);
    let mut expected_cf2 = can_frame(0x7E0, &[0x22]);
    expected_cf2.extend_from_slice(&payload[13..20]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_ff,
        "the full multi-frame payload must still reach the wire despite the cancel"
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_cf1
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 2),
        expected_cf2
    );

    // The cancellation was ignored: normal Online/Finished, not Cancelled.
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    // Nothing went wrong from the client's perspective either -- the
    // force-completed transmit must not surface as a PduErrorEvent.
    assert!(
        !wait_for_event(&mut events, 200, |item| matches!(
            item.data,
            Some(event_item::Data::ErrorData(_))
        ))
        .await,
        "no PduErrorEvent should be emitted for a non-cancellable StopComm transmit"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-085 round 7: `handle_stop_comm`'s pre-transmit `still_on_this_channel`
/// re-check (ported from `handle_start_comm`'s identical guard) suppresses a
/// StopComm final transmit when a concurrent `DisconnectComLogicalLink` races
/// its (non-cancellable) `CP_P3Phys` gap wait. Mirrors
/// `send_type_1_disconnect_racing_the_p3_gap_wait_suppresses_the_send`'s
/// structure (`tester_present_send_type.rs`) for the StopComm analog of that
/// race, reusing `p3_gap.rs`'s seed-a-no-response-send-then-wait technique to
/// force a real, several-hundred-millisecond wait inside `handle_stop_comm`'s
/// own `wait_for_p3_gap` call. cll_b shares the physical channel purely to
/// keep the poll task alive after cll_a disconnects (disconnecting the sole
/// CLL on a channel tears the channel, and the poll task, down entirely).
#[tokio::test]
#[serial]
async fn stopcomm_disconnect_racing_the_p3_gap_wait_suppresses_the_final_transmit() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000), (CP_P3_PHYS, 300_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;

    // Kept alive (not disconnected) so the shared physical channel -- and its
    // poll task -- survives cll_a's disconnect below.
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

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events).await;

    // Seed the shared CP_P3Phys gap bucket: a physically-addressed,
    // no-response CoptSendrecv from cll_a. This is what makes the upcoming
    // CoptStopcomm's own final transmit have to wait ~300ms inside
    // wait_for_p3_gap before it can run.
    send_data(&mut client, cll_a, vec![0x3E, 0x00], vec![]).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    let final_payload = vec![0x82, 0xF1, 0x01];
    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload.clone(),
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, non-empty cop_data) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;

    // Comfortably inside the ~300ms CP_P3Phys wait: the poll task should be
    // asleep inside handle_stop_comm's own (non-cancellable) wait_for_p3_gap
    // call for cll_a's final transmit right now.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // A genuinely concurrent disconnect: this runs on a different tokio task
    // than the channel poll task, and DisconnectComLogicalLink mutates
    // `logical_links` directly, outside the poll task's TxItem queue.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    // Past the full 300ms CP_P3Phys deadline (measured from the seed send)
    // with margin: wait_for_p3_gap has returned Ready by now, giving the
    // pre-transmit re-check a chance to run (and, pre-fix, giving a stale
    // final transmit a chance to go out).
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the StopComm final payload must never have been written -- cll_a disconnected while its \
         own CoptStopcomm final transmit was waiting on CP_P3Phys -- only the seed send should be \
         present"
    );

    // The COP must observe PduCopstCancelled instead of silently vanishing or
    // completing as PduCopstFinished with a phantom transmit. Checked BEFORE
    // the PduCllstOnline absence check below: wait_for_event drains (and
    // discards) every non-matching event it reads while waiting, so checking
    // absence-of-Online first would silently consume this already-arrived
    // PduCopstCancelled (emitted by DisconnectComLogicalLink's own
    // cancel_link_cops call, well before this point) without ever seeing it.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle))
        .await,
        "the StopComm COP should have received PduCopstCancelled instead of PduCopstFinished \
         after the disconnect race"
    );

    assert!(
        !wait_for_event(&mut events, 200, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "no PduCllstOnline should be emitted for cll_a: DisconnectComLogicalLink already took it \
         to PduCllstOffline, and the pre-transmit guard must not follow it with a stale Online"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-085 round 7: `handle_stop_comm`'s terminal-block `still_on_this_channel`
/// re-check (folded into the same `logical_links` lock acquisition as the
/// `comm_started`/`stop_comm_pending` write-back) detects a
/// `DisconnectComLogicalLink` that raced the (non-cancellable) multi-frame
/// ISO-TP transmit and lands before the terminal status is emitted. Reuses
/// `stopcomm_multiframe_cancel_during_transmit_does_not_abort`'s
/// FlowControl-injection window (`can_mode.rs`'s established technique) to
/// force a real, controllable pause mid-transmit -- the FirstFrame and first
/// ConsecutiveFrame are already on the wire (genuinely "written", not merely
/// queued) when the disconnect lands, and, being non-cancellable (round 6),
/// the transmit runs to completion regardless, reaching the terminal block
/// afterward with the full payload already written. `cancel_link_cops`
/// (driven by the disconnect) wins the first-wins race for
/// `PduCopstCancelled`, so `handle_stop_comm`'s own terminal-block guard must
/// not emit a duplicate terminal event.
#[tokio::test]
#[serial]
async fn stopcomm_disconnect_racing_multiframe_transmit_skips_terminal_online_and_finished() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;

    // Kept alive (not disconnected) so the shared physical channel -- and its
    // poll task -- survives cll_a's disconnect below.
    let _cll_b = create_and_connect_cll(
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

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events).await;

    // 20-byte payload -> FirstFrame (6 bytes) + 2 ConsecutiveFrames (7 bytes
    // each), the same segmentation `can_mode.rs`'s software-isotp tests pin.
    let payload: Vec<u8> = (1..=20).collect();
    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: payload.clone(),
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, non-empty cop_data) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;

    // FirstFrame goes out immediately.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // FlowControl #1: ContinueToSend, BlockSize = 1 (one ConsecutiveFrame per
    // block, forcing a SECOND FlowControl wait before the send completes).
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x30, 0x01, 0x00]),
        j2534_0404::CAN,
    );

    // The first ConsecutiveFrame goes out, then isotp_send blocks waiting for
    // the second block's FlowControl -- genuinely mid-transmit, with data
    // already written to the wire.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    // A genuinely concurrent disconnect while the transmit is blocked waiting
    // on FlowControl #2: non-cancellable (round 6), so this must not abort
    // the remaining ConsecutiveFrame.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    // Give the poll task a moment to observe (and, per round 6, ignore) the
    // disconnect before unblocking it -- if the transmit were abortable, it
    // would stop here with no further frames ever written.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "the second ConsecutiveFrame must not go out before its own FlowControl arrives"
    );

    // FlowControl #2 unblocks the final ConsecutiveFrame -- the transmit now
    // runs to completion despite the disconnect, reaching handle_stop_comm's
    // terminal block afterward.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x30, 0x00, 0x00]),
        j2534_0404::CAN,
    );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;

    // The COP must observe PduCopstCancelled -- emitted by
    // DisconnectComLogicalLink's own cancel_link_cops call, well before the
    // transmit even completed -- not PduCopstFinished from handle_stop_comm's
    // own terminal block. Checked BEFORE the duplicate-event absence checks
    // below (wait_for_event drains and discards non-matching events, so
    // checking absence first would silently consume this one).
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle))
        .await,
        "the StopComm COP should have received PduCopstCancelled from the disconnect's own \
         cancel_link_cops call, not PduCopstFinished from handle_stop_comm's own terminal block"
    );

    // No duplicate terminal event from handle_stop_comm's own terminal
    // block: no PduCllstOnline (the disconnect already took cll_a to
    // PduCllstOffline)...
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "no PduCllstOnline should be emitted for cll_a after the disconnect race"
    );
    // ...and no second CopStatus event of any kind for this same cop_handle
    // (the first-wins primitives.remove in handle_stop_comm's terminal block
    // must see cancel_link_cops already won and stay silent).
    assert!(
        !wait_for_event(&mut events, 200, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(_))
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle))
        .await,
        "no duplicate terminal status should be emitted for the StopComm cop_handle from \
         handle_stop_comm's own terminal block"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-086: `handle_stop_comm`'s pre-transmit `still_on_this_channel` guard
/// checking `channel_id` equality alone is not enough on a *shared* physical
/// channel (`ref_count > 1`): disconnecting cll_a does not tear the channel
/// down (cll_b keeps it alive), and a same-protocol/baud reconnect of the
/// SAME `cll_handle` rejoins with the identical `ChannelId` -- so a stale
/// StopComm COP's `channel_id == Some(ctx.channel_id)` check would
/// incorrectly pass again after the reconnect, letting it transmit cll_a's
/// stale final payload and emit a spurious extra `PduCllstOnline`/
/// `PduCopstFinished` for cll_a's brand-new session. `connect_generation`
/// (bumped on every `ConnectComLogicalLink`, including this reconnect) closes
/// that gap: the guard's captured value no longer matches the live one even
/// though `channel_id` matches again.
///
/// Reuses `stopcomm_disconnect_racing_the_p3_gap_wait_suppresses_the_final_transmit`'s
/// `CP_P3Phys`-gap-seeding technique to force a real, controllable pause
/// inside `handle_stop_comm`'s own `wait_for_p3_gap` call, but disconnects
/// AND immediately reconnects cll_a (same `cll_handle`, same protocol/baud,
/// rejoining the same shared channel) while the pause is in flight, instead
/// of just disconnecting.
#[tokio::test]
#[serial]
async fn stopcomm_disconnect_then_reconnect_same_channel_suppresses_stale_final_transmit() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000), (CP_P3_PHYS, 800_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;

    // Kept alive (not disconnected) so the shared physical channel -- and its
    // poll task -- survives cll_a's disconnect/reconnect below.
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

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events).await;

    // Seed the shared CP_P3Phys gap bucket: a physically-addressed,
    // no-response CoptSendrecv from cll_a. This is what makes the upcoming
    // CoptStopcomm's own final transmit have to wait ~800ms inside
    // wait_for_p3_gap before it can run.
    send_data(&mut client, cll_a, vec![0x3E, 0x00], vec![]).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    let final_payload = vec![0x82, 0xF1, 0x01];
    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload.clone(),
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, non-empty cop_data) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;

    // Comfortably inside the ~800ms CP_P3Phys wait: the poll task should be
    // asleep inside handle_stop_comm's own (non-cancellable) wait_for_p3_gap
    // call for cll_a's final transmit right now. CP_P3Phys is set well above
    // the sibling `stopcomm_disconnect_racing_the_p3_gap_wait_suppresses_the_
    // final_transmit` test's 300ms: this test does TWO sequential RPC round
    // trips below (disconnect, then reconnect) instead of that test's one,
    // and each round trip is budgeted against the measured
    // `GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS` under load, so the
    // remaining margin needs to comfortably absorb roughly double the
    // overhead (observed 2026-07-24:
    // this test failed once in 3 full parallel-suite runs with the original
    // 300ms/150ms margin, which left only ~150ms of headroom for those two
    // round trips).
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // A genuinely concurrent disconnect, immediately followed by a reconnect
    // of the SAME cll_handle with the SAME protocol/baud -- rejoining the
    // same shared physical channel (still kept alive by cll_b) and getting
    // back the identical ChannelId, but a freshly-bumped connect_generation.
    // Both run on a different tokio task than the channel poll task, and
    // mutate `logical_links` directly, outside the poll task's TxItem queue.
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
    // PduCopstCancelled for the stale StopComm COP. Checked BEFORE the
    // absence checks below: wait_for_event drains (and discards) every
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
            .is_some_and(|h| h.cop_handle == stop_cop_handle))
        .await,
        "the StopComm COP should have received PduCopstCancelled instead of PduCopstFinished \
         after the disconnect race"
    );

    // The reconnect's own ConnectComLogicalLink success legitimately emits
    // exactly one PduCllstOnline for cll_a's new session -- consume it here
    // so the later absence check only catches an extra, stale one.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "the reconnect itself should emit exactly one PduCllstOnline for cll_a's new session"
    );

    // Past the full 800ms CP_P3Phys deadline (measured from the seed send)
    // with margin: wait_for_p3_gap has returned Ready by now, giving the
    // pre-transmit re-check a chance to run (and, pre-fix, giving a stale
    // final transmit -- and a spurious duplicate Online/Finished for cll_a's
    // new session -- a chance to go out).
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the stale StopComm final payload must never have been written -- cll_a disconnected \
         and reconnected onto the same shared channel while its own CoptStopcomm final transmit \
         was waiting on CP_P3Phys -- only the seed send should be present"
    );

    assert!(
        !wait_for_event(&mut events, 200, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle))
        .await,
        "no PduCopstFinished should be emitted for the stale StopComm cop_handle from \
         handle_stop_comm's own guard -- it already lost the first-wins race to \
         cancel_link_cops's PduCopstCancelled"
    );

    assert!(
        !wait_for_event(&mut events, 200, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "no second PduCllstOnline should be emitted for cll_a: the reconnect's own Online was \
         already consumed above, so a stale handle_stop_comm terminal write-back for the OLD \
         session must not follow it with a spurious duplicate"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-086 (round 5, Codex Finding 2): a queued `CoptStopcomm` that goes
/// stale while still sitting in the poll task's FIFO -- because the
/// disconnect+reconnect that should invalidate it lands on a *different*
/// physical channel than the one still holding it behind a `CoptDelay` --
/// must be silently skipped by the pre-dispatch `should_skip_cancelled_item`
/// check (via the new `TxItem::connect_generation()`), never dispatched into
/// `handle_stop_comm`. Without this fix, `handle_stop_comm` unconditionally
/// steals/clears `LogicalLinkState.tester_present_state` (and would call
/// `stop_periodic_message` for a `Periodic` one) *before* its own in-handler
/// `still_on_this_channel` guard runs -- corrupting whatever NEW session
/// happens to occupy that same `cll_handle`'s `tester_present_state` field by
/// the time the stale item finally dispatches, even though that guard
/// correctly prevents the spurious `PduCllstOnline`/`PduCopstFinished`
/// commit itself (a narrower, round-4 fix that does not close this gap).
///
/// cll_a's `CoptStopcomm` is queued behind a `CoptDelay` on cll_b that pins
/// their shared ORIGINAL physical channel's poll task busy for `DELAY_MS`
/// (on cll_b because accepting a `CoptStopcomm` cancels every other COP on
/// its own CLL, a running `CoptDelay` included). Disconnecting
/// cll_a immediately cancels that queued StopComm (`cancel_link_cops`,
/// `PduCopstCancelled`), but the `CoptDelay` itself keeps that channel's poll
/// task -- and the stale StopComm still parked behind it in the same FIFO --
/// alive (cll_b, kept connected throughout, keeps the ORIGINAL physical
/// channel itself from tearing down). cll_a is then reconnected at a
/// DIFFERENT `DATA_RATE`, landing on a brand-new physical channel with its
/// own, independent poll task -- unblocked by the original channel's still-
/// sleeping `CoptDelay`. A fresh `CoptStartcomm` on that new channel arms a
/// NEW `tester_present_state` (idle-triggered, `CP_TesterPresentSendType =
/// 1`) with an idle deadline set safely past `DELAY_MS`, so the ORIGINAL
/// channel's stale StopComm dispatches (and, pre-fix, tears down state)
/// before that deadline arrives -- deterministically distinguishing "state
/// survived" (the idle-triggered second frame eventually fires) from "state
/// was nuked" (it never does, and `wait_for_written_count` times out).
#[tokio::test]
#[serial]
async fn stopcomm_stale_queued_item_does_not_tear_down_a_reconnected_sessions_tester_present() {
    const DELAY_MS: u32 = 300;
    // Comfortably longer than DELAY_MS (plus the margin slept below) so the
    // stale StopComm's (pre-fix) teardown -- if it happens at all -- lands
    // well before this deadline, making the two outcomes deterministically
    // distinguishable.
    const IDLE_INTERVAL_US: u32 = 900_000; // 900ms
    const NEW_CHANNEL_ID: u32 = 2;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    // A plain protocol_id (rather than a resources-table resource_id) gets no
    // preset ComParam defaults (see CreateComLogicalLink's "legacy fallback"
    // path) -- functional addressing and the tester-present message must be
    // set explicitly, mirroring
    // `send_type_1_disconnect_then_reconnect_same_channel_suppresses_stale_arm`'s
    // setup above.
    set_com_param_unum32(&mut client, cll_a, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_a, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    // ADR-138: tester-present's own addressing ComParam, independent of
    // CP_RequestAddrMode above.
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_bytes(&mut client, cll_a, CP_TESTER_PRESENT_MESSAGE, vec![0x3E]).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_SEND_TYPE, 1).await;
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_TIME, IDLE_INTERVAL_US).await;
    // ADR-137: CP_TesterPresentHandling's ISO_15765_4 spec default is 0
    // (disabled); this test needs the ORIGINAL channel's tester-present
    // state actually armed to distinguish "survived" from "was nuked", so
    // stage the promotion explicitly.
    set_com_param_unum32(&mut client, cll_a, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for cll_a");

    // Kept alive (not disconnected) so cll_a's ORIGINAL physical channel --
    // and its poll task, still busy with the CoptDelay below -- survives
    // cll_a's disconnect/reconnect-elsewhere.
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

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events).await;

    // Hold cll_a's ORIGINAL channel's poll task busy for DELAY_MS.
    queue_delay(&mut client, cll_b, DELAY_MS).await;

    // Queued behind the CoptDelay above -- not yet dispatched.
    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) should succeed (queued behind CoptDelay)")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
        .cop_handle;

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    // cancel_link_cops's own immediate cancellation of the still-queued
    // StopComm. Checked before any further wait_for_event calls so it is not
    // silently discarded (drained and ignored) by one of them.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle))
        .await,
        "the queued StopComm COP should have received PduCopstCancelled from cancel_link_cops"
    );

    // Reconnect cll_a at a DIFFERENT DATA_RATE -- a distinct physical
    // channel/poll task from cll_b's (and from the one still holding the
    // stale StopComm behind the CoptDelay).
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

    // A fresh CoptStartcomm on the NEW channel -- unblocked by the ORIGINAL
    // channel's still-sleeping CoptDelay -- arms a brand-new
    // tester_present_state for cll_a's new session.
    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events).await;

    // ADR-084: the new arm's own immediate tester-present frame.
    wait_for_written_count(&server, NEW_CHANNEL_ID, 1).await;

    // Past DELAY_MS with margin: the ORIGINAL channel's poll task has by now
    // finished its CoptDelay and reached the stale, already-cancelled
    // StopComm -- pre-fix, this is where it would steal/clear cll_a's (now
    // NEW-session) tester_present_state before its own in-handler
    // still_on_this_channel guard gets a chance to bail out.
    tokio::time::sleep(std::time::Duration::from_millis(u64::from(DELAY_MS) + 200)).await;

    // No further CopStatus event of any kind -- in particular, no
    // PduCopstExecuting -- for the stale StopComm's cop_handle:
    // should_skip_cancelled_item's generation check must have skipped it
    // before handle_stop_comm ever ran.
    assert!(
        !wait_for_event(&mut events, 200, |item| item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle))
        .await,
        "no further CopStatus event should be emitted for the stale StopComm cop_handle -- \
         handle_stop_comm must never run for it"
    );

    // The idle-triggered second frame for cll_a's NEW session must still
    // fire: proof tester_present_state was never touched by the stale
    // StopComm dispatching on the OLD channel while it was in flight.
    wait_for_written_count(&server, NEW_CHANNEL_ID, 2).await;

    drop(events);
    server.shutdown().await;
}

/// ADR-087: `NumReceiveCycles` on a non-empty-`cop_data` `CoptStopcomm` is
/// now honored instead of silently ignored. `-1` (IS-CYCLIC) is rejected
/// synchronously: an "until cancelled" receive contradicts a COP that must
/// terminate to return the ComLogicalLink to `PDU_CLLST_ONLINE`. The COP is
/// never enqueued, so `stop_comm_pending` is rolled back and `comm_started`
/// stays `true` -- a follow-up `CoptStopcomm` succeeds normally.
#[tokio::test]
#[serial]
async fn stopcomm_num_receive_cycles_is_cyclic_rejected_synchronously() {
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![0x82, 0xF1, 0x01],
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
        .expect_err("num_receive_cycles == -1 (IS-CYCLIC) should be rejected for CoptStopcomm");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("-1"), "{}", status.message());
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    // comm is still started, and stop_comm_pending was rolled back: a
    // follow-up (valid) CoptStopcomm succeeds normally.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "a valid CoptStopcomm immediately after the rejected IS-CYCLIC attempt should \
             succeed -- stop_comm_pending must have been rolled back",
        );
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// `NumReceiveCycles < -2` on a non-empty-`cop_data` `CoptStopcomm` is
/// rejected synchronously, mirroring `CoptSendrecv`'s own validation.
#[tokio::test]
#[serial]
async fn stopcomm_num_receive_cycles_below_minus_two_rejected_synchronously() {
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![0x82, 0xF1, 0x01],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -3,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect_err("num_receive_cycles below -2 should be rejected for CoptStopcomm");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("num_receive_cycles"),
        "{}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "a valid CoptStopcomm immediately after the rejected attempt should succeed -- \
             stop_comm_pending must have been rolled back",
        );
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// ADR-087: a `CoptStopcomm` with non-empty `cop_data` and a non-empty
/// `expected_response_array`/`NumReceiveCycles > 0` runs the same bounded
/// receive phase `CoptSendrecv` runs after its own transmit -- a matching
/// ECU response is delivered via `ResultData` with the descriptor's
/// `acceptance_id`, attributed to the StopComm's own `cop_handle`, before
/// the normal `PduCllstOnline`/`PduCopstFinished` teardown sequence.
#[tokio::test]
#[serial]
async fn stopcomm_expected_response_delivers_result_data_before_teardown() {
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

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    // A KWP2000-shaped StopCommunication request, expecting a positive
    // response (first payload byte 0xC2) delivered under acceptance_id 7.
    let final_payload = vec![0x82, 0xF1, 0x01];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC2],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, expected_response_array) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Neither PduCllstOnline nor PduCopstFinished should appear yet: the
    // receive phase is still waiting for the matching response.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "PduCllstOnline must not be emitted before the expected response is delivered"
    );

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0xC2, 0xF1, 0x01]),
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

    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// ADR-087, Decision point 2: the receive phase `handle_stop_comm` runs after
/// its final transmit is `cancellable: false`, exactly like round 6's
/// transmit-phase rule. A `CancelComPrimitive` on the StopComm's own
/// `cop_handle`, arriving while that receive phase is genuinely still
/// waiting for the matching response, must NOT produce `PduCopstCancelled`
/// for it -- the cancel is silently ignored (the entry it leaves in
/// `cancelled_cops` is reaped later by `dispatch_tx_item`'s existing
/// post-completion cleanup, per the `ExpectedResponseWait::cancellable` doc
/// comment) -- and the COP must still reach its normal terminal
/// `PduCllstOnline`/`PduCopstFinished` sequence once the matching response
/// arrives. Reuses `stopcomm_expected_response_delivers_result_data_before_teardown`'s
/// setup (unique_resp_table + `NumReceiveCycles = 1` + a response pattern
/// the ECU has not sent yet) to force a real, still-waiting receive phase to
/// cancel into.
#[tokio::test]
#[serial]
async fn stopcomm_cancel_during_receive_phase_does_not_cancel_still_reaches_terminal() {
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

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    let final_payload = vec![0x82, 0xF1, 0x01];
    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC2],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, expected_response_array) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Genuinely still waiting: no PduCllstOnline yet -- same technique the
    // preceding test uses to prove the receive phase has not resolved before
    // the cancel below is issued.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "PduCllstOnline must not be emitted before the expected response is delivered"
    );

    // CancelComPrimitive on the StopComm's own cop_handle while its receive
    // phase is genuinely mid-wait.
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(stop_cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    // No PduCopstCancelled for this cop_handle: the receive phase is
    // non-cancellable (ADR-087 Decision point 2) -- the cancel above must be
    // silently ignored rather than surfaced as a status event.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle.cop_handle))
        .await,
        "CancelComPrimitive during the StopComm's receive phase must not produce \
         PduCopstCancelled -- the receive phase is non-cancellable (ADR-087)"
    );

    // GetStatus(COP) polled right now must not report Cancelled either --
    // the ignored cancel must not leave a stale `cancelled_cops` marker for
    // `rpc_get_status`'s CopHandle branch to trust (Codex review, PR #92:
    // that branch checks `cancelled_cops` before `executing_cop`, Priority
    // Cancelled > Executing > Waiting, so a lingering marker would make
    // GetStatus lie for as long as the receive phase's own P2Max/ceiling
    // window takes, not just "transiently").
    let status = client
        .get_status(vci_service_interface::GetStatusRequest {
            handle: Some(
                vci_service_interface::get_status_request::Handle::CopHandle(stop_cop_handle),
            ),
        })
        .await
        .expect("get_status(COP) should succeed")
        .into_inner();
    assert!(
        !matches!(
            status.status,
            Some(vci_service_interface::status_response::Status::CopStatus(s))
                if s == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ),
        "GetStatus(COP) must not report PduCopstCancelled for an ignored cancel during a \
         non-cancellable StopComm receive phase -- the COP is guaranteed to keep running and \
         later emit PduCopstFinished, got {:?}",
        status.status
    );

    // The matching response now arrives -- the receive phase completes
    // normally despite the earlier (ignored) cancel.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0xC2, 0xF1, 0x01]),
        j2534_0404::ISO15765,
    );

    // Terminal sequence must still occur: PduCllstOnline + PduCopstFinished,
    // not PduCopstCancelled.
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// ADR-087: a `DisconnectComLogicalLink` racing a StopComm's receive phase
/// (`NumReceiveCycles = 1`, still genuinely waiting for the matching
/// response) is a DIFFERENT race than the cancel above -- disconnect's own
/// `cancel_link_cops` call unconditionally cancels every COP still tracked
/// in `primitives` for the CLL, bypassing `cancelled_cops`/`cancellable`
/// entirely (same first-wins idiom
/// `stopcomm_disconnect_racing_multiframe_transmit_skips_terminal_online_and_finished`
/// exercises for the transmit-phase analog of this race, generalized here to
/// land inside the NEW receive-phase code path specifically, with
/// `NumReceiveCycles = 1` instead of that test's `0`). Asserts: the StopComm
/// COP gets `PduCopstCancelled` (first-wins, from `cancel_link_cops`), no
/// `PduCllstOnline` follows for the stale session, and `stop_comm_pending`
/// was cleared -- proven by a fresh reconnect + `CoptStartcomm` +
/// `CoptStopcomm` cycle succeeding afterward.
#[tokio::test]
#[serial]
async fn stopcomm_disconnect_during_receive_phase_first_wins_cancelled_no_online_clears_pending() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
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

    // Kept alive (not disconnected) so the shared physical channel -- and its
    // poll task -- survives cll_a's disconnect below, exactly like the
    // existing round-7 disconnect-race tests in this file.
    let _cll_b = create_and_connect_cll(
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

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events).await;

    let final_payload = vec![0x82, 0xF1, 0x01];
    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC2],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, expected_response_array) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Genuinely still waiting: no PduCllstOnline yet.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "PduCllstOnline must not be emitted before the expected response is delivered"
    );

    // A genuinely concurrent disconnect while the receive phase is still
    // waiting for the matching response.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    // The StopComm COP must observe PduCopstCancelled -- emitted by
    // DisconnectComLogicalLink's own cancel_link_cops call. Checked BEFORE
    // the absence checks below: wait_for_event drains (and discards) every
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
            .is_some_and(|h| h.cop_handle == stop_cop_handle.cop_handle))
        .await,
        "the StopComm COP should have received PduCopstCancelled from the disconnect's own \
         cancel_link_cops call"
    );

    // No PduCllstOnline for cll_a: the disconnect already took it to
    // PduCllstOffline, and the receive phase's own staleness guard must not
    // follow it with a stale Online.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "no PduCllstOnline should be emitted for cll_a after the disconnect race"
    );

    // No duplicate CopStatus event of any kind for the StopComm's cop_handle:
    // the receive phase's own first-wins staleness guard must see
    // cancel_link_cops already won and stay silent.
    assert!(
        !wait_for_event(&mut events, 200, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(_))
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle.cop_handle))
        .await,
        "no duplicate terminal status should be emitted for the StopComm cop_handle"
    );

    // stop_comm_pending was cleared by the disconnect: reconnecting cll_a and
    // running a fresh CoptStartcomm -> CoptStopcomm cycle must succeed, not
    // be rejected "already in progress".
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("reconnecting cll_a should succeed");
    wait_for_cll_online(&mut events).await;

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events).await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "a follow-up CoptStopcomm after the reconnect should succeed -- stop_comm_pending \
             must have been cleared by the disconnect that raced the earlier StopComm's receive \
             phase",
        );
    wait_for_cll_online(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// ADR-086 (this round, Codex review of PR #92 / ADR-087's own guard):
/// `poll_rx_inner`'s `MatchProbe` attribution arm was keyed on
/// `entry.handle == p.target_cll` alone -- no `connect_generation` check --
/// so a same-channel disconnect+reconnect of the SAME `cll_handle`,
/// completing while a non-cancellable receive phase (`CoptStopcomm` here,
/// `CoptSendrecv` shares the identical gap) is still polling, could
/// misattribute a fresh session's very first frame as `ResultData`/
/// `cop_handle` to the OLD, stale COP. `wait_for_expected_response`'s own
/// per-pass staleness check runs at the loop bottom, AFTER that same pass's
/// attribution decision already ran -- it narrows but cannot close this
/// window, since `.await`s (`build_cll_rx_entries`'s own `logical_links`
/// lock, `ctx.api.lock()`) sit in between.
///
/// cll_a and cll_b share one physical channel (cll_b kept connected
/// throughout to keep the channel/poll task alive across cll_a's
/// disconnect+reconnect). cll_a starts a `CoptStopcomm` with
/// `NumReceiveCycles = 1` and an `expected_response_array` entry against a
/// mock ECU that has not yet answered, forcing a genuine, still-polling
/// receive phase (mirroring `stopcomm_disconnect_during_receive_phase_
/// first_wins_cancelled_no_online_clears_pending`'s setup). cll_a is then
/// disconnected (`cancel_link_cops` immediately emits `PduCopstCancelled`
/// for the StopComm's `cop_handle`) and immediately reconnected at the SAME
/// `DATA_RATE`, rejoining the identical shared `ChannelId` with a
/// freshly-bumped `connect_generation` while the stale wait loop is still
/// running. A matching response frame is then injected on the shared
/// physical channel: the stale loop's very next `poll_rx_and_check_match`
/// pass (up to one `POLL_INTERVAL_MS` tick later) sees it, and — since
/// attribution happens before that SAME pass's own staleness check —
/// this is a deterministic, non-flaky reproduction, not a scheduler race.
///
/// Under ADR-100's unbound-discard rule (Decision §5), `cll_a`'s own copy of
/// the injected frame is now genuinely unbound once the stale StopComm
/// registrant is excluded by `connect_generation` -- nothing else is
/// listening on `cll_a` itself, so it is silently discarded rather than
/// delivered. This test therefore asserts that discard directly (no
/// `ResultData` at all reaches `cll_a`'s own subscription -- in particular
/// not one carrying the stale `cop_handle`, which is what a misattribution
/// bug would produce) instead of the pre-ADR-100 shape of "delivered, but
/// check the `cop_handle`". `cll_b` carries a receive-only monitor
/// (`arm_receive_only_monitor`) armed once, up front, well before the
/// race-critical section -- since a created-receive-only `-1` registrant now
/// detaches to tier 2 immediately on insertion (ADR-100 Decision §2's
/// immediate-detach fix, this ADR's own gap closed), arming it costs nothing
/// beyond its own RPC round trip and never blocks `cll_a`'s later
/// `CoptStartcomm`/`CoptStopcomm` dispatch on the shared channel. It stays
/// live, unaffected by `cll_a`'s disconnect/reconnect, and independently
/// observes the SAME physical frame -- proving routing/delivery itself
/// survived the disconnect+reconnect intact and the frame's absence on
/// `cll_a` is specifically `cll_a`'s own (correct) attribution exclusion, not
/// a hardware/routing failure.
///
/// No further `CopStatus` follows for the stale `cop_handle` — the wait's
/// own staleness check still terminates it, silently, once it runs, exactly
/// like the disconnect-during-receive-phase sibling test above.
#[tokio::test]
#[serial]
async fn stopcomm_receive_phase_reconnect_mid_wait_does_not_misattribute_new_frame_to_stale_cop() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
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

    // Kept alive (not disconnected) so the shared physical channel -- and
    // its poll task, still busy with cll_a's stale StopComm wait below --
    // survives cll_a's disconnect/reconnect.
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

    // cll_b has no UniqueRespIdTable of its own, so `arm_receive_only_
    // monitor`'s dummy, never-transmitted payload cannot resolve physical
    // (table-based) TX addressing (see that helper's own doc comment).
    // Functional addressing resolves independently of the UniqueRespIdTable
    // (ADR-054) and does not affect RX routing (which keys purely off
    // `active_unique_resp_id_table`, unset here) -- so this satisfies TX
    // resolution without giving cll_b a table.
    set_com_param_unum32(&mut client, cll_b, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_b, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    promote_via_update_param(&mut client, cll_b).await;

    // Armed once, here, well before cll_a's own StartComm/StopComm sequence
    // (and long before the race-critical disconnect+reconnect+inject section
    // below): a created-receive-only `-1` registrant now detaches to tier 2
    // the instant it is inserted (this ADR's own gap fix), so this call
    // never blocks the shared channel's poll task waiting for its own first
    // match -- cll_a's later dispatch is unaffected. cll_b is never
    // disconnected, so this registrant survives cll_a's whole
    // disconnect/reconnect sequence untouched.
    arm_receive_only_monitor(&mut client, cll_b).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a).await;
    wait_for_cop_finished(&mut events).await;

    let final_payload = vec![0x82, 0xF1, 0x01];
    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC2],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, expected_response_array) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Genuinely still waiting: no PduCllstOnline yet.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "PduCllstOnline must not be emitted before the expected response is delivered"
    );

    // A genuinely concurrent disconnect (cancel_link_cops immediately
    // cancels the still-executing StopComm) followed immediately by a
    // reconnect of the SAME cll_handle at the SAME DATA_RATE -- rejoining
    // the identical shared ChannelId with a freshly-bumped
    // connect_generation -- and, with ZERO intervening event-stream reads
    // (each of which is an `.await` the single-threaded test runtime could
    // use to run the stale wait loop's own per-pass staleness check to
    // completion, closing the window before this test ever gets to inject
    // anything), an immediate response-frame injection right after the
    // reconnect RPC returns. This keeps the whole disconnect+reconnect+
    // inject sequence to the minimum number of `.await` points, maximizing
    // the chance the stale wait loop's own ~`POLL_INTERVAL_MS` (10ms) sleep
    // has not yet elapsed by the time the frame lands -- so its next poll
    // pass sees BOTH the bumped generation AND the matching frame at once,
    // which is exactly the interleaving that misattributes without the
    // `connect_generation` guard. Every event this sequence produces
    // (`PduCopstCancelled`, `PduCllstOnline`) is drained from the stream
    // afterward instead, once the injection has already happened.
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
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0xC2, 0xF1, 0x01]),
        j2534_0404::ISO15765,
    );

    // Now drain the events the disconnect+reconnect legitimately produced.
    // Checked in arrival order: wait_for_event drains (and discards) every
    // non-matching event it reads while waiting, so checking out of order
    // would silently consume an event before its own check runs.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle.cop_handle))
        .await,
        "the StopComm COP should have received PduCopstCancelled from the disconnect's own \
         cancel_link_cops call"
    );
    wait_for_cll_online(&mut events).await;

    // ADR-100 Decision §5's unbound-discard rule: the stale StopComm
    // registrant is excluded by connect_generation and nothing else is
    // listening on cll_a itself, so the injected frame is genuinely unbound
    // for cll_a and silently discarded -- no ResultData at all should reach
    // cll_a's own subscription. A misattribution bug (the thing this test
    // exists to catch) would instead deliver a ResultData here carrying the
    // stale cop_handle, so asserting NONE arrives is a strictly stronger
    // check than the old "delivered, but check cop_handle" shape.
    assert!(
        !wait_for_event(&mut events, 500, |item| matches!(
            item.data,
            Some(event_item::Data::ResultData(_))
        ))
        .await,
        "no ResultData should reach cll_a's own subscription -- the injected frame is genuinely \
         unbound for cll_a (the stale StopComm registrant is excluded by connect_generation) and \
         must be discarded, not misattributed to the stale cop_handle"
    );

    // No further CopStatus for the stale StopComm cop_handle -- in
    // particular no PduCopstFinished (which the misattributed match would
    // otherwise have driven the receive phase toward) and no second
    // PduCopstCancelled: the wait loop's own per-pass staleness check still
    // terminates it silently once it runs, since cancel_link_cops already
    // won the first-wins race for this cop_handle at disconnect time.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(_))
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == stop_cop_handle.cop_handle))
        .await,
        "no further CopStatus should be emitted for the stale StopComm cop_handle"
    );

    // cll_b's own live monitor -- armed once, up front, unaffected by cll_a's
    // disconnect/reconnect -- independently observes the SAME physical
    // frame: routing/delivery itself survived the disconnect+reconnect
    // intact, so cll_a's silence above is specifically its own attribution
    // exclusion at work, not a hardware/routing failure swallowing the frame
    // entirely.
    let cll_b_result = wait_for_result_data(&mut client, cll_b).await;
    assert_result_data(
        &cll_b_result,
        &0x7E8_u32.to_be_bytes(),
        &[],
        &[0xC2, 0xF1, 0x01],
    );

    drop(events);
    server.shutdown().await;
}

/// Codex review, PR #92: `CoptStopcomm`'s non-cancellable IS-MULTIPLE
/// (`NumReceiveCycles = -2`) receive phase used to reset its per-match
/// deadline on every accepted response with no absolute cap -- a chatty ECU
/// (or a broad/empty `expected_response` pattern) could keep it running
/// indefinitely, and since `CancelComPrimitive` is deliberately ignored for
/// this phase (ADR-087), there was no escape short of a disconnect.
/// `ExpectedResponseWait::match_reset_ceiling_ms`, anchored once at
/// receive-phase entry, now clamps every per-match deadline extension to
/// `max(16 x CP_P2Max, 2000ms)` (`STOPCOMM_IS_MULTIPLE_CEILING_FACTOR`/
/// `STOPCOMM_IS_MULTIPLE_CEILING_FLOOR_MS`,
/// j2534-0404-service/src/service/events.rs). With `CP_P2Max` set to 50 ms
/// here, `16 x 50ms = 800ms` is below the 2000ms floor, so the ceiling is
/// exactly the floor. A mock ECU that keeps answering every 20 ms -- faster
/// than the 50 ms window could naturally close -- must still see the receive
/// phase end at roughly that 2 s mark rather than run indefinitely, and --
/// because at least one response arrived -- must not see
/// `PduErrEvtRxTimeout` (a ceiling-driven window close is indistinguishable
/// from a normal IS-MULTIPLE window close).
#[tokio::test]
#[serial]
async fn stopcomm_is_multiple_chatty_ecu_bounded_by_match_reset_ceiling() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 50_000), // 50 ms window -> ceiling is the 2000ms floor
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    let final_payload = vec![0x82, 0xF1, 0x01];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -2,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC2],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, NumReceiveCycles = -2) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Chatter a matching response every 20 ms -- faster than the 50 ms
    // window can naturally close -- while polling events, for up to 2.5 s: a
    // generous bound above the ~2 s ceiling that still fails fast (rather
    // than hanging the suite) if the clamp regresses, since the ECU keeps
    // chattering until 3 s, well past this loop's 2.5 s hard stop.
    let start = tokio::time::Instant::now();
    let chatter_end = start + tokio::time::Duration::from_millis(3_000);
    let hard_stop = start + tokio::time::Duration::from_millis(2_500);
    let mut finished = false;
    let mut saw_rx_timeout = false;
    let mut saw_online = false;
    let mut match_count = 0u32;
    while tokio::time::Instant::now() < hard_stop {
        if tokio::time::Instant::now() < chatter_end {
            server.backdoor.inject_rx(
                MOCK_CHANNEL_ID,
                &can_frame(0x7E8, &[0xC2, 0xF1, 0x01]),
                j2534_0404::ISO15765,
            );
        }
        let remaining = hard_stop.saturating_duration_since(tokio::time::Instant::now());
        let step = tokio::time::Duration::from_millis(20).min(remaining);
        match tokio::time::timeout(step, events.message()).await {
            Ok(Ok(Some(notification))) => {
                let Some(vci_service_interface::event_notification::EventData::Item(item)) =
                    notification.event_data
                else {
                    continue;
                };
                match &item.data {
                    Some(event_item::Data::ResultData(_)) => match_count += 1,
                    Some(event_item::Data::ErrorData(err))
                        if *err
                            == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32 =>
                    {
                        saw_rx_timeout = true;
                    }
                    Some(event_item::Data::CllStatus(status))
                        if *status
                            == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline
                                as i32 =>
                    {
                        saw_online = true;
                    }
                    Some(event_item::Data::CopStatus(status))
                        if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
                    {
                        finished = true;
                    }
                    _ => {}
                }
                if finished {
                    break;
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) => break, // stream ended or errored
            Err(_) => {}                        // this step's poll timed out; loop and inject again
        }
    }
    let elapsed = start.elapsed();

    assert!(
        finished,
        "the chatty-ECU IS-MULTIPLE receive phase should end at roughly the \
         match_reset_ceiling_ms bound (~2s), not run indefinitely; elapsed = {elapsed:?}"
    );
    assert!(
        elapsed <= tokio::time::Duration::from_millis(2_600),
        "elapsed {elapsed:?} should be close to the ~2s ceiling, not stretch toward the full 3s \
         chatter window"
    );
    assert!(
        match_count > 0,
        "at least one matching response should have been collected"
    );
    assert!(
        !saw_rx_timeout,
        "matches_got > 0 -- a ceiling-driven window close is not a receive timeout"
    );
    assert!(
        saw_online,
        "PduCllstOnline should be emitted once the receive phase ends"
    );

    drop(events);
    server.shutdown().await;
}

/// `stopcomm_is_multiple_chatty_ecu_bounded_by_match_reset_ceiling` above
/// uses `CP_P2Max = 50ms`, where `16 x 50 = 800ms < 2000ms`, so
/// `STOPCOMM_IS_MULTIPLE_CEILING_FLOOR_MS` dominates every assertion there
/// and `STOPCOMM_IS_MULTIPLE_CEILING_FACTOR` (16x) is never actually
/// exercised -- a regression that broke the factor specifically (wrong
/// multiplier, or `.max`/`.saturating_mul` operands swapped) would pass that
/// test untouched (edge-case-hunter finding, PR #92 review round).  This
/// test uses `CP_P2Max = 200ms`, where `16 x 200 = 3200ms > 2000ms`, so the
/// factor -- not the floor -- determines the ceiling.
#[tokio::test]
#[serial]
async fn stopcomm_is_multiple_chatty_ecu_bounded_by_ceiling_factor_not_floor() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 200_000), // 200 ms window -> ceiling is 16x200=3200ms, not the 2000ms floor
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    let final_payload = vec![0x82, 0xF1, 0x01];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -2,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC2],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, NumReceiveCycles = -2) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Chatter a matching response every 80 ms -- faster than the 200 ms
    // window can naturally close -- for up to 4.5 s (well past the ~3.2 s
    // factor-derived ceiling), while polling events up to a 3.9 s hard stop:
    // generous enough above the ~3.2 s ceiling to tolerate scheduling
    // jitter, but well short of the 4.5 s the ECU keeps chattering for, so a
    // regression back to the 2 s floor (too short) or an unbounded wait
    // (never finishes) both fail this test distinctly from a pass at ~3.2 s.
    let start = tokio::time::Instant::now();
    let chatter_end = start + tokio::time::Duration::from_millis(4_500);
    let hard_stop = start + tokio::time::Duration::from_millis(3_900);
    let mut finished = false;
    let mut saw_rx_timeout = false;
    let mut saw_online = false;
    let mut match_count = 0u32;
    while tokio::time::Instant::now() < hard_stop {
        if tokio::time::Instant::now() < chatter_end {
            server.backdoor.inject_rx(
                MOCK_CHANNEL_ID,
                &can_frame(0x7E8, &[0xC2, 0xF1, 0x01]),
                j2534_0404::ISO15765,
            );
        }
        let remaining = hard_stop.saturating_duration_since(tokio::time::Instant::now());
        let step = tokio::time::Duration::from_millis(80).min(remaining);
        match tokio::time::timeout(step, events.message()).await {
            Ok(Ok(Some(notification))) => {
                let Some(vci_service_interface::event_notification::EventData::Item(item)) =
                    notification.event_data
                else {
                    continue;
                };
                match &item.data {
                    Some(event_item::Data::ResultData(_)) => match_count += 1,
                    Some(event_item::Data::ErrorData(err))
                        if *err
                            == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32 =>
                    {
                        saw_rx_timeout = true;
                    }
                    Some(event_item::Data::CllStatus(status))
                        if *status
                            == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline
                                as i32 =>
                    {
                        saw_online = true;
                    }
                    Some(event_item::Data::CopStatus(status))
                        if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
                    {
                        finished = true;
                    }
                    _ => {}
                }
                if finished {
                    break;
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) => break, // stream ended or errored
            Err(_) => {}                        // this step's poll timed out; loop and inject again
        }
    }
    let elapsed = start.elapsed();

    assert!(
        finished,
        "the chatty-ECU IS-MULTIPLE receive phase should end at roughly the \
         factor-derived ceiling (~3.2s = 16 x 200ms), not run indefinitely; elapsed = {elapsed:?}"
    );
    assert!(
        elapsed >= tokio::time::Duration::from_millis(2_600),
        "elapsed {elapsed:?} ending near the 2000ms floor instead of the ~3.2s factor-derived \
         ceiling would indicate the 16x factor is not actually being applied"
    );
    assert!(
        elapsed <= tokio::time::Duration::from_millis(3_900),
        "elapsed {elapsed:?} should be close to the ~3.2s factor-derived ceiling, not stretch \
         toward the full 4.5s chatter window"
    );
    assert!(
        match_count > 0,
        "at least one matching response should have been collected"
    );
    assert!(
        !saw_rx_timeout,
        "matches_got > 0 -- a ceiling-driven window close is not a receive timeout"
    );
    assert!(
        saw_online,
        "PduCllstOnline should be emitted once the receive phase ends"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-102: `match_reset_ceiling_ms` (the same clamp
/// `stopcomm_is_multiple_chatty_ecu_bounded_by_match_reset_ceiling` above
/// exercises for plain matches) also bounds the RC78 (NRC 0x78,
/// ResponsePending) auto-handling reload path, not just the ordinary
/// per-match deadline reset -- a chattering ECU that never sends a genuine
/// match, only a stream of 0x78, must not be able to hold the
/// non-cancellable IS-MULTIPLE receive phase open indefinitely by
/// continuously reloading `CP_P2Star`'s deadline (ADR-102's reload-per-
/// occurrence fix). `CP_RC78CompletionTimeout` is deliberately left unset
/// here, isolating this to `match_reset_ceiling_ms` alone: with `CP_P2Max`
/// at 50 ms, `16 x 50ms = 800ms` is below the 2000ms floor
/// (`STOPCOMM_IS_MULTIPLE_CEILING_FLOOR_MS`), so the ceiling is exactly the
/// floor, same as the plain chatty-ECU test above. Because no genuine match
/// ever arrives, the phase must end via `PduErrEvtRxTimeout`, not a normal
/// window close.
#[tokio::test]
#[serial]
async fn stopcomm_is_multiple_rc78_chatter_bounded_by_match_reset_ceiling() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 50_000), // 50 ms window -> ceiling is the 2000ms floor
            (CP_RC78_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 2),
            // CP_RC78CompletionTimeout deliberately left unset: isolates
            // this test to match_reset_ceiling_ms alone.
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    let final_payload = vec![0x82, 0xF1, 0x01];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -2,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC2],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, NumReceiveCycles = -2) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Chatter NRC 0x78 (ResponsePending, at RC-byte-offset 2) every 20 ms --
    // never a genuine match -- for up to 3 s: a generous bound above the
    // ~2 s ceiling that still fails fast if the RC78 reload path escapes
    // the clamp.
    let start = tokio::time::Instant::now();
    let chatter_end = start + tokio::time::Duration::from_millis(3_000);
    let hard_stop = start + tokio::time::Duration::from_millis(2_500);
    let mut finished = false;
    let mut saw_rx_timeout = false;
    let mut match_count = 0u32;
    while tokio::time::Instant::now() < hard_stop {
        if tokio::time::Instant::now() < chatter_end {
            server.backdoor.inject_rx(
                MOCK_CHANNEL_ID,
                &can_frame(0x7E8, &[0x7F, 0x82, 0x78]),
                j2534_0404::ISO15765,
            );
        }
        let remaining = hard_stop.saturating_duration_since(tokio::time::Instant::now());
        let step = tokio::time::Duration::from_millis(20).min(remaining);
        match tokio::time::timeout(step, events.message()).await {
            Ok(Ok(Some(notification))) => {
                let Some(vci_service_interface::event_notification::EventData::Item(item)) =
                    notification.event_data
                else {
                    continue;
                };
                match &item.data {
                    // acceptance_id 7 is this COP's own descriptor; a bound
                    // (known-ECU) frame that the RC78 handler consumes
                    // instead of matching still surfaces to the CLL
                    // subscription as a plain ResultData with acceptance_id
                    // 0 (undirected delivery, not a genuine COP match) --
                    // only the former counts toward `match_count` here.
                    Some(event_item::Data::ResultData(rd)) if rd.acceptance_id == 7 => {
                        match_count += 1;
                    }
                    Some(event_item::Data::ErrorData(err))
                        if *err
                            == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32 =>
                    {
                        saw_rx_timeout = true;
                    }
                    Some(event_item::Data::CopStatus(status))
                        if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
                    {
                        finished = true;
                    }
                    _ => {}
                }
                if finished {
                    break;
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) => break, // stream ended or errored
            Err(_) => {}                        // this step's poll timed out; loop and inject again
        }
    }
    let elapsed = start.elapsed();

    assert!(
        finished,
        "the RC78-chattering IS-MULTIPLE receive phase should end at roughly the \
         match_reset_ceiling_ms bound (~2s), not run indefinitely; elapsed = {elapsed:?}"
    );
    assert!(
        elapsed <= tokio::time::Duration::from_millis(2_600),
        "elapsed {elapsed:?} should be close to the ~2s ceiling, not stretch toward the full 3s \
         chatter window"
    );
    assert_eq!(
        match_count, 0,
        "no genuine match (acceptance_id 7) was ever injected -- only 0x78 chatter"
    );
    assert!(
        saw_rx_timeout,
        "with no genuine match ever collected, the ceiling-driven window close IS a receive \
         timeout, unlike the plain chatty-ECU ceiling test above"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-102 companion to the RC78 test above: `match_reset_ceiling_ms` also
/// bounds the RC21 (NRC 0x21, BusyRepeatRequest) auto re-request path's own
/// `CP_RC21RequestTime` sleep and the retransmit that follows it, not just
/// the terminal deadline write -- a client-configured `CP_RC21RequestTime`
/// longer than the ceiling must not hold the non-cancellable IS-MULTIPLE
/// receive phase open past it. With `CP_P2Max` at 50 ms the ceiling is the
/// ~2 s floor (as above); `CP_RC21RequestTime` is set to 3 s, deliberately
/// longer. A single 0x21 is injected once: the ceiling must elapse
/// mid-sleep, ending the phase at ~2 s (not ~3 s+) with the retransmit
/// correctly skipped -- exactly one write to the mock backdoor (the
/// original StopComm final message), never a second.
#[tokio::test]
#[serial]
async fn stopcomm_is_multiple_rc21_request_time_bounded_by_match_reset_ceiling() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 50_000), // 50 ms window -> ceiling is the 2000ms floor
            (CP_RC21_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 2),
            // 3000 ms request-time sleep (stored in us, ADR-057) --
            // deliberately longer than the ~2000ms ceiling.
            (CP_RC21_REQUEST_TIME, 3_000_000),
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    let final_payload = vec![0x82, 0xF1, 0x01];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -2,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC2],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, NumReceiveCycles = -2) should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // A single NRC 0x21 (BusyRepeatRequest) at RC-byte-offset 2 -- no
    // further frames are ever injected, so any second write to the mock
    // backdoor could only be the RC21 retransmit.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x82, 0x21]),
        j2534_0404::ISO15765,
    );

    let mut finished = false;
    let mut saw_rx_timeout = false;
    let mut match_count = 0u32;
    let start = tokio::time::Instant::now();
    let hard_stop = start + tokio::time::Duration::from_millis(3_200);
    while tokio::time::Instant::now() < hard_stop {
        let remaining = hard_stop.saturating_duration_since(tokio::time::Instant::now());
        let step = tokio::time::Duration::from_millis(50).min(remaining);
        match tokio::time::timeout(step, events.message()).await {
            Ok(Ok(Some(notification))) => {
                let Some(vci_service_interface::event_notification::EventData::Item(item)) =
                    notification.event_data
                else {
                    continue;
                };
                match &item.data {
                    // acceptance_id 7 is this COP's own descriptor; a bound
                    // (known-ECU) frame that the RC21 handler consumes
                    // instead of matching still surfaces to the CLL
                    // subscription as a plain ResultData with acceptance_id
                    // 0 (undirected delivery, not a genuine COP match) --
                    // only the former counts toward `match_count` here.
                    Some(event_item::Data::ResultData(rd)) if rd.acceptance_id == 7 => {
                        match_count += 1;
                    }
                    Some(event_item::Data::ErrorData(err))
                        if *err
                            == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32 =>
                    {
                        saw_rx_timeout = true;
                    }
                    Some(event_item::Data::CopStatus(status))
                        if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
                    {
                        finished = true;
                    }
                    _ => {}
                }
                if finished {
                    break;
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) => break, // stream ended or errored
            Err(_) => {}                        // this step's poll timed out
        }
    }
    let elapsed = start.elapsed();

    assert!(
        finished,
        "the receive phase should end at roughly the match_reset_ceiling_ms bound (~2s), not \
         run indefinitely; elapsed = {elapsed:?}"
    );
    assert!(
        elapsed <= tokio::time::Duration::from_millis(2_600),
        "elapsed {elapsed:?} should be close to the ~2s ceiling, not stretch toward the full \
         3s CP_RC21RequestTime"
    );
    assert_eq!(
        match_count, 0,
        "no genuine match (acceptance_id 7) was ever injected -- only a single 0x21"
    );
    assert!(
        saw_rx_timeout,
        "with no genuine match ever collected, the ceiling-driven window close IS a receive \
         timeout"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the RC21 retransmit must be skipped once the ceiling elapses mid-sleep -- only the \
         original StopComm final message should have been written"
    );

    drop(events);
    server.shutdown().await;
}

/// Closes the backlog
/// entry citing ADR-087: proves the RC21 (NRC 0x21, `BusyRepeatRequest`)
/// auto re-request path's own retransmit-failure handling
/// (`TxFailure::Event(PduErrEvtTxError)` -> `wait_for_expected_response_inner`
/// -> `ReceivePhaseOutcome::ReRequestTxFailed`) is reachable and handled
/// correctly for `CoptStopcomm`'s own caller (`handle_stop_comm`): per
/// ADR-087, that caller falls through to its own terminal
/// `PduCllstOnline`/`PduCopstFinished` block on this outcome, after the
/// TX-error event already emitted inside the wait. This is genuinely
/// discriminating -- if `ReRequestTxFailed`'s handling were removed/broken,
/// the COP would either hang (never reaching that terminal block) or never
/// surface the `PduErrEvtTxError` event at all.
///
/// Companion to `rc_handling.rs`'s
/// `rc21_retransmit_write_failure_still_finishes_the_cop_sendrecv` (the
/// `CoptSendrecv` half of this same backlog entry). Same write-failure
/// override mechanism (`__mock_set_write_msgs_error`), armed *before* the
/// RC21 is injected -- the original StopComm final message's own first
/// write (before arming) must succeed, since the ECU needs something to
/// NRC-reject, but once armed the override applies to every subsequent
/// `PassThruWriteMsgs` call regardless of timing, so this test has no race
/// against `CP_RC21RequestTime`'s (here, unset/zero, i.e. no sleep)
/// retry-sleep scheduling.
#[tokio::test]
#[serial]
async fn stopcomm_rc21_retransmit_write_failure_still_reaches_terminal() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            // Comfortably longer than the 2000ms `wait_for_event` window
            // below (Codex review finding, PR #123): a P2_MAX exactly equal
            // to the assertion window would let a regression that keeps
            // waiting instead of handling `ReRequestTxFailed` still produce
            // a terminal event inside that same window via the P2 timeout
            // itself, making this test pass for the wrong reason.
            (j2534_0404::P2_MAX, 10_000_000),
            (CP_RC21_HANDLING, 1),
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

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    let final_payload = vec![0x82, 0xF1, 0x01];
    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC2],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, expected_response_array) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Arm the write-failure override before the RC21 is ever injected: the
    // retransmit it triggers is then guaranteed to fail, with no timing
    // race against CP_RC21RequestTime's retry-sleep scheduling.
    server
        .backdoor
        .set_write_msgs_error(Some(j2534_0404::ERR_FAILED as std::os::raw::c_long));

    // NRC 0x21 (BusyRepeatRequest) at RC-byte-offset 2. SID 0x82 echoes
    // `final_payload`'s own first byte (ADR-100 Decision §3, resolved (b)'s
    // request-SID gate).
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x82, 0x21]),
        j2534_0404::ISO15765,
    );

    let mut saw_tx_error = false;
    assert!(
        wait_for_event(&mut events, 2_000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtTxError as i32
            ) {
                saw_tx_error = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ) && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == stop_cop_handle.cop_handle)
        })
        .await,
        "the RC21 retransmit-failure path should still reach handle_stop_comm's own terminal \
         PduCopstFinished, not hang"
    );
    assert!(
        saw_tx_error,
        "the failed RC21 retransmit should surface PduErrEvtTxError (TxFailure::Event -> \
         ReceivePhaseOutcome::ReRequestTxFailed, ADR-087)"
    );

    server.backdoor.set_write_msgs_error(None);
    drop(events);
    server.shutdown().await;
}

/// Companion to `stopcomm_cancel_during_receive_phase_does_not_cancel_still_reaches_terminal`:
/// that test proved `GetStatus(COP)` doesn't lie once the receive phase has
/// reached its own per-pass check. This test proves it doesn't lie *during*
/// the RC21 (NRC 0x21) `request_time_ms` sleep either -- a `.await` inside
/// the same per-pass iteration that a `cancellable: false` StopComm receive
/// phase used to leave un-drained until the sleep (plus the retransmit that
/// follows it) completed, which could stretch `GetStatus`'s false
/// `PduCopstCancelled` window to the full, client-configured
/// `CP_RC21RequestTime` (Codex review, PR #92, second round of this finding).
#[tokio::test]
#[serial]
async fn stopcomm_get_status_not_cancelled_during_rc21_request_time_sleep() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 3_000_000),
            (CP_RC21_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 2),
            // 300 ms request-time sleep (stored in us, ADR-057) -- long
            // enough for the mid-sleep CancelComPrimitive/GetStatus pair
            // below to land comfortably inside it.
            (CP_RC21_REQUEST_TIME, 300_000),
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

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    let final_payload = vec![0x82, 0xF1, 0x01];
    let stop_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 7,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0xC2],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, expected_response_array) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // NRC 0x21 (BusyRepeatRequest) at RC-byte-offset 2 -- triggers the RC21
    // auto re-request path: a `request_time_ms` sleep, then a retransmit of
    // the original StopComm final message. SID 0x82 echoes `final_payload`'s
    // own first byte (ADR-100 Decision §3, resolved (b)'s request-SID gate).
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x82, 0x21]),
        j2534_0404::ISO15765,
    );

    // Land in the middle of the 300 ms request-time sleep.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(stop_cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    // The drain runs once per `POLL_INTERVAL_MS` (10 ms) chunk boundary, not
    // continuously -- give it a few chunks' worth of margin (50 ms) so this
    // assertion checks "the sleep drains its cancel promptly," not "the
    // exact instant after the cancel RPC returns," which would otherwise
    // flakily race whichever chunk boundary happens to be nearest.
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Poll GetStatus(COP), still well inside the 300 ms request-time sleep:
    // it must not report
    // Cancelled, even though the ignored cancel's cancelled_cops entry
    // exists (rpc_get_status checks it before executing_cop) -- the sleep
    // must drain it on every POLL_INTERVAL_MS chunk, not only once the
    // sleep (and the retransmit after it) finally completes.
    let status = client
        .get_status(vci_service_interface::GetStatusRequest {
            handle: Some(
                vci_service_interface::get_status_request::Handle::CopHandle(stop_cop_handle),
            ),
        })
        .await
        .expect("get_status(COP) should succeed")
        .into_inner();
    assert!(
        !matches!(
            status.status,
            Some(vci_service_interface::status_response::Status::CopStatus(s))
                if s == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ),
        "GetStatus(COP) must not report PduCopstCancelled mid-sleep during the RC21 \
         request_time_ms wait -- the ignored cancel must be drained on every sleep chunk, not \
         only at the per-pass check after the sleep (and retransmit) complete, got {:?}",
        status.status
    );

    // Let the retransmit actually go out (confirms the sleep+re-request
    // sequence ran to completion), then deliver the real positive response
    // so the receive phase -- and the whole StopComm -- completes normally
    // despite the earlier (ignored) cancel.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0xC2, 0xF1, 0x01]),
        j2534_0404::ISO15765,
    );

    assert!(
        wait_for_event(&mut events, 1_000, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline as i32
        ))
        .await,
        "PduCllstOnline should still be emitted once the (retransmitted) response arrives"
    );

    drop(events);
    server.shutdown().await;
}

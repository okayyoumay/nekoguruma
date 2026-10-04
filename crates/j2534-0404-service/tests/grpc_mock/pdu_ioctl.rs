//! The 17 D-PDU-style `PDU_IOCTL_*` adapter commands (ADR-079):
//! `GetObjectId(OBJT_IO_CTRL, ...)` name resolution feeding straight into
//! `IoCtl`, TX-suspend/resume FIFO ordering, and the four commands this
//! adapter rejects as unsupported by explicit product decision.

use serial_test::serial;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest, DataItem,
    DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest, ExpectedResponseData,
    GetEventItemRequest, GetObjectIdRequest, GetTimestampRequest, IoBytearray, IoCtlRequest,
    IoEventQueueProperty, IoFilter, IoFilterList, LockResourceRequest, ModuleConnectRequest,
    ModuleHandle, ObjectType, PduComLogicalLinkStatus, PduComPrimitiveStatus, PduError, PduFilter,
    PduQueueMode, StartComPrimitiveRequest, SubscribeEventRequest, UnlockResourceRequest,
    data_item, error_detail_from_status, event_item, event_notification, get_event_item_request,
    io_ctl_request, subscribe_event_request, vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)` -- the real client-facing discovery path,
/// not a hardcoded literal.
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

async fn io_ctl_module(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cmd_id: u32,
) -> Result<(), tonic::Status> {
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data: None,
            has_output: false,
        })
        .await
        .map(|_| ())
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

/// Starts a fire-and-forget `CoptSendrecv` (no response expected) and returns
/// immediately -- `StartComPrimitive` only enqueues the `TxItem`, so this does
/// not wait for the poll task to dispatch it (matching `cop_ctrl_cycles.rs`'s
/// `start_send_recv`).
async fn start_send_recv(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    cop_data: Vec<u8>,
) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");
}

/// The 4 M/L-target commands with no underlying hardware capability on this
/// adapter (explicit product decision, not an oversight, ADR-079): each
/// rejects with `Status::unimplemented` and the exact `PDU_ERR_*` message
/// prefix, once the module/CLL is connected (this test always connects
/// first; see `module_scoped_unsupported_ioctls_reject_when_module_not_connected`
/// in this same file for the pre-connect `PDU_ERR_MODULE_NOT_CONNECTED`
/// case the 3 module-scoped commands here -- `GENERIC`/`GET_CABLE_ID`/
/// `READ_IGNITION_SENSE_STATE` -- now also observe, A2-8).
#[tokio::test]
#[serial]
async fn unsupported_commands_reject_with_the_documented_status_and_message() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let generic_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GENERIC").await;
    let status = io_ctl_module(&mut client, generic_id)
        .await
        .expect_err("PDU_IOCTL_GENERIC should be rejected");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert!(status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"));
    assert!(status.message().contains("PDU_IOCTL_GENERIC"));
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrIdNotSupported as i32
    );

    let cable_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_CABLE_ID").await;
    let status = io_ctl_module(&mut client, cable_id)
        .await
        .expect_err("PDU_IOCTL_GET_CABLE_ID should be rejected");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert!(status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"));
    assert!(status.message().contains("PDU_IOCTL_GET_CABLE_ID"));
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrIdNotSupported as i32
    );

    let ignition_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_IGNITION_SENSE_STATE").await;
    let status = io_ctl_module(&mut client, ignition_id)
        .await
        .expect_err("PDU_IOCTL_READ_IGNITION_SENSE_STATE should be rejected");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert!(status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"));
    assert!(
        status
            .message()
            .contains("PDU_IOCTL_READ_IGNITION_SENSE_STATE")
    );
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrIdNotSupported as i32
    );

    let send_break_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SEND_BREAK").await;
    let status = io_ctl_cll(&mut client, cll_handle, send_break_id)
        .await
        .expect_err("PDU_IOCTL_SEND_BREAK should be rejected");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert!(status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"));
    assert!(status.message().contains("PDU_IOCTL_SEND_BREAK"));
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrIdNotSupported as i32
    );

    // An unrecognized numeric io_ctrl_command_id BELOW the ADR-219 vendor
    // range (neither a D-PDU-style adapter command nor one of the 4 legacy
    // raw J2534 ids) falls through rpc_io_ctl to rpc_io_ctl_legacy's own
    // unrecognized-cmd_id fallback -- a distinct code path from the 4
    // assertions above, and one that used to return a bare
    // Status::unimplemented with no ErrorDetail at all (Codex review, round
    // 10). `0x9999` (not `0x9999_0000`, ADR-219 round of this test):
    // `cmd_id >= 0x10000` is now routed to the new vendor-IOCTL passthrough
    // dispatch instead (see `tests/grpc_mock/vendor_passthrough.rs`), so
    // this regression case must stay below that boundary to keep exercising
    // `rpc_io_ctl_legacy`'s own catch-all.
    let status = io_ctl_cll(&mut client, cll_handle, 0x9999)
        .await
        .expect_err("an unrecognized numeric IOCTL command should be rejected");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert!(status.message().contains("0x00009999"));
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrIdNotSupported as i32
    );

    server.shutdown().await;
}

/// `PDU_IOCTL_SUSPEND_TX_QUEUE` holds every subsequently dispatched `TxItem`
/// for this CLL in `tx_held` instead of executing it; `PDU_IOCTL_RESUME_TX_QUEUE`
/// drains `tx_held` in FIFO order, so the mock sees the three requests in the
/// exact order they were submitted, only after resume.
#[tokio::test]
#[serial]
async fn suspend_then_resume_preserves_fifo_order() {
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

    // Three distinct payloads, submitted in this order.
    start_send_recv(&mut client, cll_handle, vec![0x01]).await;
    start_send_recv(&mut client, cll_handle, vec![0x02]).await;
    start_send_recv(&mut client, cll_handle, vec![0x03]).await;

    // Give the poll task ample time to dequeue and park all three -- none
    // should reach the mock while suspended.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "no request should be written while the TX queue is suspended"
    );

    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        can_frame(0x7E0, &[0x01]),
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        can_frame(0x7E0, &[0x02]),
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 2),
        can_frame(0x7E0, &[0x03]),
    );

    server.shutdown().await;
}

/// Codex-review regression (round 9): reproduces the exact race
/// `PDU_IOCTL_RESUME_TX_QUEUE`'s old "re-send held items to the tail of the
/// shared queue" mechanism got wrong -- a CLL's own item still sitting
/// un-dequeued in the shared `tx_queue` at the moment of resume ended up
/// running BEFORE an older item of the same CLL that had already been
/// diverted into `tx_held`, inverting per-CLL FIFO order.
///
/// Deterministic reproduction, using the same "long `CoptDelay` pins the FIFO
/// queue open" technique as `param_binding.rs`'s ADR-067 tests, applied
/// across two CLLs sharing one physical channel (as in
/// `p3_gap.rs::phys_gap_is_scoped_to_the_shared_channel_across_clls`):
///
/// 1. `cll_a` and `cll_b` connect to the same physical channel
///    (`connect_count() == 1`).
/// 2. `cll_a` is suspended, then submits item A -- the queue is empty at
///    that point, so A is dequeued and diverted into `cll_a`'s `tx_held`
///    almost immediately (settled by a short sleep).
/// 3. `cll_b` (never suspended) submits a long `CoptDelay`, which the poll
///    task dequeues and blocks on for its full duration -- deterministically
///    holding the shared queue open, exactly as in `param_binding.rs`.
/// 4. While the poll task is still blocked in that delay, `cll_a` submits
///    item B: it is guaranteed to sit un-dequeued behind the in-flight delay
///    when resume fires next, reproducing the race window the bug
///    description calls out ("another CLL's items were ahead of them in the
///    interleaved FIFO stream").
/// 5. `cll_a` is resumed while B is still queued behind the delay.
///
/// Under the old "resume re-sends held items to the tail" mechanism, A would
/// land in the queue behind the already-queued B, so B would be written
/// first -- inverting the submission order. The fix instead lets B dequeue
/// normally (no longer suspended) and drains `tx_held` (containing A) the
/// moment the poll loop sees B belongs to `cll_a`, so A is written first.
#[tokio::test]
#[serial]
async fn resume_does_not_invert_fifo_order_against_a_still_queued_own_item() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;

    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E1).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;

    io_ctl_cll(&mut client, cll_a, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    // Item A: the queue is empty, so this is dequeued and diverted into
    // cll_a's tx_held almost immediately. Settle before moving on.
    start_send_recv(&mut client, cll_a, vec![0x01]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "item A should be held, not written, while cll_a is suspended"
    );

    // A long CoptDelay on cll_b (never suspended) pins the shared queue open
    // deterministically: the poll task dequeues it immediately and blocks
    // for its full duration, exactly as in param_binding.rs's ADR-067 tests.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptDelay as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 300,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptDelay) should succeed");

    // Item B: submitted while the poll task is blocked in cll_b's delay, so
    // it is guaranteed to still be sitting un-dequeued in the shared queue,
    // behind the in-flight delay, at the moment resume is called next.
    start_send_recv(&mut client, cll_a, vec![0x02]).await;

    io_ctl_cll(&mut client, cll_a, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        can_frame(0x7E0, &[0x01]),
        "item A (already in tx_held before resume) must be written before item B, even though B \
         was still sitting in the shared queue when resume was called"
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        can_frame(0x7E0, &[0x02]),
    );

    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 13, fix O): the `parked`
/// due-item dispatch section must also drain a CLL's `tx_held` backlog before
/// dispatching a due cyclic follow-up cycle (ADR-053) -- the same rule
/// `drain_tx_held_backlog` already enforces on the `tx_rx.recv()` branch
/// (ADR-081), but exercised here on the *other* call site: a due parked
/// continuation is dispatched directly from the `parked` loop, bypassing
/// `tx_rx.recv()` entirely.
///
/// Deterministic construction, reusing the same "long `CoptDelay` on a
/// second CLL pins the shared poll loop open" technique as
/// `resume_does_not_invert_fifo_order_against_a_still_queued_own_item` above:
///
/// 1. `cll_a` starts a cyclic `CoptSendrecv` (`NumSendCycles = 2`,
///    `Time = 300` ms): cycle 1 dispatches immediately (frame `[0x01]`) and
///    parks a follow-up cycle due 300 ms later (ADR-053).
/// 2. `cll_a` is suspended once cycle 1 has been confirmed written -- after
///    the follow-up is parked but nowhere near its 300 ms due time.
/// 3. A fresh, non-cyclic item (`[0x02]`) is submitted on `cll_a` while
///    suspended; the poll loop's own `tx_rx.recv()` branch dequeues and
///    diverts it into `tx_held` (settled by a short sleep, mirroring the
///    suspend/resume tests above).
/// 4. `cll_b` (sharing the same physical channel, never suspended) submits a
///    long `CoptDelay` that the poll loop dequeues and blocks on for its
///    full duration, deterministically pinning the dispatch loop busy.
/// 5. While the loop is blocked in that delay, `cll_a` is resumed: this
///    clears `tx_suspended` and enqueues a `ResumeWake` -- but the loop
///    cannot dequeue it until the delay finishes.
/// 6. The follow-up's 300 ms due time elapses while the loop is still
///    blocked in `cll_b`'s (longer) delay.
///
/// When the delay finally finishes, the poll loop's outer iteration checks
/// `parked` for due items *before* it ever gets a chance to service the
/// still-undequeued `ResumeWake`. Without this fix, the due follow-up cycle
/// would dispatch straight from the `parked` loop -- writing cycle 2's frame
/// -- before the older `[0x02]` item sitting in `tx_held` ever gets drained,
/// inverting per-CLL FIFO order. With the fix, the `parked` loop drains
/// `tx_held` first, so `[0x02]` is written before cycle 2's follow-up frame.
#[tokio::test]
#[serial]
async fn parked_due_item_drains_tx_held_backlog_before_a_cyclic_followup() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;

    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E1).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;

    // Cyclic COP: cycle 1 dispatches immediately, cycle 2 (the follow-up)
    // is parked, due 300 ms later.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 300,
                num_send_cycles: 2,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(cyclic CoptSendrecv) should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    io_ctl_cll(&mut client, cll_a, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    // Fresh item on cll_a, submitted while suspended -- settles into
    // tx_held almost immediately.
    start_send_recv(&mut client, cll_a, vec![0x02]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the fresh item should be held, not written, while cll_a is suspended"
    );

    // A long CoptDelay on cll_b (never suspended) pins the shared poll loop
    // busy for its full duration, deterministically preventing it from
    // servicing tx_rx.recv() (and thus the ResumeWake below) until it
    // finishes -- exactly as in the round 9 regression above.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptDelay as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 500,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptDelay) should succeed");

    // Give the poll loop a moment to dequeue and enter the delay before
    // resuming cll_a -- the ResumeWake must land while the loop is
    // demonstrably busy, not before.
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    io_ctl_cll(&mut client, cll_a, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    // At this point the cyclic follow-up (due 300 ms after cycle 1) has
    // long since come due, but cll_b's 500 ms delay is still blocking the
    // poll loop's dispatch section, so the ResumeWake remains undequeued.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        can_frame(0x7E0, &[0x01]),
        "cycle 1 of the cyclic COP"
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        can_frame(0x7E0, &[0x02]),
        "the item held in tx_held must be drained before the due cyclic follow-up runs, even \
         though the follow-up came due while the loop was blocked servicing cll_b's delay"
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 2),
        can_frame(0x7E0, &[0x01]),
        "cycle 2 (the parked follow-up), dispatched only after tx_held was drained"
    );

    server.shutdown().await;
}

/// Verified bug fix 1 regression: a CLL's client-installed message filter
/// (`PDU_IOCTL_START_MSG_FILTER`) must be stopped via `PassThruStopMsgFilter`
/// when the CLL disconnects -- otherwise the filter keeps matching on
/// hardware after the CLL that owns it is gone.
///
/// Originally adapted (round 3) so the filter installed on `cll_a` while it
/// was still the sole owner, with `cll_b` joining the same channel only
/// afterward, to preserve coverage of disconnect-time cleanup while the
/// channel was shared -- without hitting round 3's own install-time
/// rejection (`start_msg_filter_rejects_on_a_shared_physical_channel`).
///
/// Round 4's fix D (`connect_com_logical_link_rejects_joining_a_channel_with_an_active_client_filter`)
/// now additionally rejects a second CLL joining a channel that already
/// carries an active client filter, so `cll_b` can no longer join `cll_a`'s
/// channel at all once the filter is installed -- combined with round 3's
/// rejection of installing a filter on an already-shared channel, a client
/// filter and a shared physical channel are now mutually exclusive states.
/// The `cll_b`/shared-channel half of this regression is therefore no longer
/// reachable through any legitimate call sequence; this test now only covers
/// the remaining, still-relevant half -- that disconnect stops the CLL's own
/// filter -- on the sole-owner channel that is the only state this scenario
/// can occur in post-fix-D.
#[tokio::test]
#[serial]
async fn disconnect_stops_this_clls_client_filter() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_a)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_START_MSG_FILTER should succeed while cll_a is still the sole owner");

    assert_eq!(
        server.backdoor.stop_filter_count(),
        0,
        "no filter should have been stopped yet"
    );

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    assert_eq!(
        server.backdoor.stop_filter_count(),
        2,
        "disconnecting cll_a should stop its own client filter -- installed as 2 hardware \
         filters (TxFlags 0 and TX_EXTENDED_ID) because raw CAN always connects CAN_ID_BOTH \
         (ADR-065)"
    );

    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 8, fix K):
/// `DestroyComLogicalLink` must stop a CLL's client-installed message filter
/// (`PDU_IOCTL_START_MSG_FILTER`) too, mirroring
/// `disconnect_stops_this_clls_client_filter` above -- `rpc_destroy_com_logical_link`
/// now holds `shared_channels` across the whole filter-teardown +
/// `ref_count` sequence (ADR-080's lock-ordering rule), and this confirms
/// that restructuring did not change the observable behavior: the filter is
/// still stopped exactly once, on a sole-owner channel (an already-shared
/// channel is unreachable here per fix D, same as the disconnect case).
#[tokio::test]
#[serial]
async fn destroy_stops_this_clls_client_filter() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_a)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_START_MSG_FILTER should succeed while cll_a is still the sole owner");

    assert_eq!(
        server.backdoor.stop_filter_count(),
        0,
        "no filter should have been stopped yet"
    );

    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("destroy_com_logical_link should succeed");

    assert_eq!(
        server.backdoor.stop_filter_count(),
        2,
        "destroying cll_a should stop its own client filter -- installed as 2 hardware \
         filters (TxFlags 0 and TX_EXTENDED_ID) because raw CAN always connects CAN_ID_BOTH \
         (ADR-065)"
    );

    server.shutdown().await;
}

/// Verified bug fix 2 regression: a CLL's held TX queue (`tx_held`, populated
/// by `PDU_IOCTL_SUSPEND_TX_QUEUE`) must be drained -- with `PduCopstCancelled`
/// emitted for every held item -- when the CLL disconnects, and
/// `tx_suspended` must be reset. Otherwise, reconnecting the same
/// `cll_handle` and later calling `PDU_IOCTL_RESUME_TX_QUEUE` would replay the
/// stale item onto the new connection even though the client already
/// received `PduCopstCancelled` for it.
#[tokio::test]
#[serial]
async fn disconnect_drains_held_tx_queue_so_a_reconnect_never_replays_it() {
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
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;

    io_ctl_cll(&mut client, cll_handle, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    start_send_recv(&mut client, cll_handle, vec![0xAA]).await;

    // Give the poll task ample time to dequeue and park the item in
    // `tx_held` -- it must never reach the mock while suspended.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "no request should be written while the TX queue is suspended"
    );

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    // The held item must be cancelled, not silently dropped.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "the held TxItem should be cancelled on disconnect"
    );

    // Verified bug fix 3 regression: a held item's cop is also still present
    // in `primitives` at cancellation time (`StartComPrimitive` inserts
    // before the poll task ever parks the item into `tx_held`), so
    // `cancel_link_cops`'s `primitives` scan and its `tx_held` drain must not
    // both emit `PduCopstCancelled` for the same cop. Assert exactly one
    // notification is emitted, not two.
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "the held TxItem must be cancelled exactly once, not twice"
    );
    drop(events);

    // Reconnect the same cll_handle -- CreateComLogicalLink was not
    // re-issued, so Working ComParams (baud rate, UniqueRespIdTable) are
    // still in place.
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for the same cll_handle after disconnect");

    // A later RESUME_TX_QUEUE must be a no-op: the stale item must never
    // replay onto the new connection.
    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the item held before disconnect must never be executed on the reconnected link"
    );

    server.shutdown().await;
}

/// Verified bug fix 1 regression (Codex PR #80): `PDU_IOCTL_RESET` must
/// drain a CLL's held TX queue (`tx_held`, populated by
/// `PDU_IOCTL_SUSPEND_TX_QUEUE`) through the same cancellation path as
/// disconnect -- emitting `PduCopstCancelled` for the held item and removing
/// it from `primitives` -- instead of silently dropping it via
/// `tx_held.clear()`, which left the client never notified and the stale
/// `primitives` entry unreachable.
#[tokio::test]
#[serial]
async fn reset_drains_held_tx_queue_and_cancels_it() {
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
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    let reset_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESET").await;

    io_ctl_cll(&mut client, cll_handle, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    start_send_recv(&mut client, cll_handle, vec![0xAA]).await;

    // Give the poll task ample time to dequeue and park the item in
    // `tx_held` -- it must never reach the mock while suspended.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "no request should be written while the TX queue is suspended"
    );

    io_ctl_module(&mut client, reset_id)
        .await
        .expect("PDU_IOCTL_RESET should succeed");

    // The held item must be cancelled, not silently dropped.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "the held TxItem should be cancelled by PDU_IOCTL_RESET"
    );
    drop(events);

    // PDU_IOCTL_RESET also resets tx_suspended -- a later RESUME_TX_QUEUE
    // must be a no-op because tx_held was already drained.
    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the item held before PDU_IOCTL_RESET must never be executed"
    );

    server.shutdown().await;
}

/// Edge-case-hunter regression (PR #129 follow-up): the only prior coverage
/// of ADR-120's `PDU_IOCTL_RESET` clock-reset amendment --
/// `events.rs`'s `module_clock_reset_tests::reset_module_clock_rebases_timestamp_backward`
/// -- calls `events::reset_module_clock()` directly, never through the real
/// `PDU_IOCTL_RESET` gRPC path. An edge-case-hunter review confirmed this is
/// a real gap, not just a theoretical one: temporarily removing the
/// `events::reset_module_clock();` call from `rpc_misc.rs::ioctl_reset` still
/// left the full 409-test `grpc_mock` suite passing. This test exercises
/// that one-line wiring end-to-end via `GetTimestamp` before and after a
/// real `PDU_IOCTL_RESET` `IoCtl` RPC.
#[tokio::test]
#[serial]
async fn reset_rebases_the_shared_module_clock_through_the_real_rpc_path() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let module_handle = ModuleHandle {
        module_handle: MOCK_MODULE_HANDLE,
    };

    // No priming call needed: the shared clock (`events::module_timestamp_us()`)
    // is now eagerly initialized at `J2534Service::new()`'s first statement
    // (`events::init_module_clock()`, ADR-120 Amendment 2), not lazily on
    // first read -- by the time any RPC reaches this handler, the clock has
    // already been running since service startup, well before this test's
    // `TestServer::start()`. Same sleep duration as events.rs's own
    // reset_module_clock_rebases_timestamp_backward unit test, to ensure the
    // clock has advanced measurably before the "before reset" reading.
    tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;

    let before_reset = client
        .get_timestamp(GetTimestampRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("get_timestamp should succeed")
        .into_inner()
        .timestamp;

    let reset_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESET").await;
    io_ctl_module(&mut client, reset_id)
        .await
        .expect("PDU_IOCTL_RESET should succeed");

    let after_reset = client
        .get_timestamp(GetTimestampRequest {
            module_handle: Some(module_handle),
        })
        .await
        .expect("get_timestamp should succeed")
        .into_inner()
        .timestamp;

    assert!(
        after_reset < before_reset,
        "expected post-reset timestamp ({after_reset}) to be smaller than pre-reset timestamp \
         ({before_reset}) -- PDU_IOCTL_RESET should rebase the shared module clock \
         (events::reset_module_clock()) through the real gRPC IoCtl path"
    );

    server.shutdown().await;
}

/// Verified bug fix 2 regression (Codex PR #80): `PDU_IOCTL_CLEAR_TX_QUEUE`
/// must drain this CLL's held TX queue (`tx_held`) through the same
/// cancellation path as disconnect -- emitting `PduCopstCancelled` for the
/// held item and removing it from `primitives` -- instead of silently
/// dropping it via `tx_held.clear()`.
#[tokio::test]
#[serial]
async fn clear_tx_queue_drains_held_tx_queue_and_cancels_it() {
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
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    let clear_tx_queue_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_CLEAR_TX_QUEUE").await;

    io_ctl_cll(&mut client, cll_handle, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    start_send_recv(&mut client, cll_handle, vec![0xAA]).await;

    // Give the poll task ample time to dequeue and park the item in
    // `tx_held` -- it must never reach the mock while suspended.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "no request should be written while the TX queue is suspended"
    );

    io_ctl_cll(&mut client, cll_handle, clear_tx_queue_id)
        .await
        .expect("PDU_IOCTL_CLEAR_TX_QUEUE should succeed");

    // The held item must be cancelled, not silently dropped.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "the held TxItem should be cancelled by PDU_IOCTL_CLEAR_TX_QUEUE"
    );
    drop(events);

    // PDU_IOCTL_CLEAR_TX_QUEUE does not resume tx dispatch -- resume it
    // explicitly; either way tx_held was already drained, so nothing replays.
    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the item held before PDU_IOCTL_CLEAR_TX_QUEUE must never be executed"
    );

    server.shutdown().await;
}

/// Verified bug fix 4 regression (Codex PR #80): if a later filter in a
/// multi-filter `PDU_IOCTL_START_MSG_FILTER` request is rejected, every
/// filter already installed on the hardware by an earlier entry in the same
/// request must be rolled back (`PassThruStopMsgFilter`) before the error is
/// returned -- otherwise it stays live on hardware but is recorded nowhere,
/// so `STOP_MSG_FILTER`/`CLEAR_MSG_FILTERS`/disconnect-time cleanup can never
/// remove it.
#[tokio::test]
#[serial]
async fn start_msg_filter_rolls_back_earlier_filters_on_a_later_failure() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // Connecting a CAN link already installs this adapter's own pass-all
    // filter(s) (`install_pass_all_filter`, ADR-005/008/039); baseline
    // against that instead of assuming a fresh mock has zero filters.
    let start_filter_count_before = server.backdoor.start_filter_count();
    let stop_filter_count_before = server.backdoor.stop_filter_count();

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![
                        // Valid: installs successfully on the mock hardware.
                        IoFilter {
                            filter_type: PduFilter::PduFltBlock as i32,
                            filter_number: 1,
                            filter_mask_message: vec![0, 0, 0, 0],
                            filter_pattern_message: vec![0, 0, 0, 0],
                        },
                        // Invalid: rejected before any hardware call is made
                        // for it, forcing a rollback of the filter installed
                        // just above.
                        IoFilter {
                            filter_type: PduFilter::PduFltUnspecified as i32,
                            filter_number: 2,
                            filter_mask_message: vec![0, 0, 0, 0],
                            filter_pattern_message: vec![0, 0, 0, 0],
                        },
                    ],
                })),
            }),
            has_output: false,
        })
        .await
        .expect_err("the second filter's PduFltUnspecified filter_type should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("PDU_ERR_INVALID_PARAMETERS"));

    assert_eq!(
        server.backdoor.start_filter_count() - start_filter_count_before,
        2,
        "only the first (valid) filter should have reached the hardware -- installed as 2 \
         hardware filters (TxFlags 0 and TX_EXTENDED_ID) because raw CAN always connects \
         CAN_ID_BOTH (ADR-065)"
    );
    assert_eq!(
        server.backdoor.stop_filter_count() - stop_filter_count_before,
        2,
        "the first filter must be rolled back (stopped) after the second one failed, or it \
         leaks live on the hardware -- both of its 2 hardware filter installs must be rolled \
         back"
    );

    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 12): a `TxFlags`-0 hardware
/// filter only matches 11-bit CAN Ids, so on a CAN channel connected with
/// `CAN_ID_BOTH` (raw `CAN` always connects `CAN_ID_BOTH`, per ADR-065 --
/// see `connect_flags.rs::raw_can_default_connect_flags_are_can_id_both`), a
/// single `PDU_FLT_BLOCK` filter must be installed once per applicable
/// `TxFlags`/ID-width variant -- mirroring `install_pass_all_filter`'s
/// existing dual-install behavior for the pass-all baseline -- or it would
/// silently never match 29-bit traffic. This asserts `start_filter_count()`
/// increases by 2 (not 1) for a single `PDU_IOCTL_START_MSG_FILTER` filter.
#[tokio::test]
#[serial]
async fn start_msg_filter_installs_once_per_tx_flags_variant_on_a_can_id_both_channel() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // Raw CAN always connects CAN_ID_BOTH regardless of configured widths
    // (ADR-065), so no extra ComParam/UniqueRespIdTable setup is needed here.
    assert_eq!(
        server.backdoor.connect_flags_log(),
        vec![0x800],
        "sanity check: raw CAN should have connected with CAN_ID_BOTH (0x800)"
    );

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let start_filter_count_before = server.backdoor.start_filter_count();

    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_START_MSG_FILTER should succeed");

    assert_eq!(
        server.backdoor.start_filter_count() - start_filter_count_before,
        2,
        "a single client filter should reach the hardware twice -- once with TxFlags 0 (11-bit) \
         and once with TX_EXTENDED_ID (29-bit) -- since the channel accepts both ID widths"
    );

    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 2): `PDU_IOCTL_CLEAR_TX_QUEUE`
/// must check `LOCK_PHYSICAL_TX_QUEUE` *before* draining/cancelling `tx_held` --
/// not after. Otherwise a caller that gets `ResourceExhausted` has already had
/// its held COP cancelled as a side effect of a call that ultimately failed.
#[tokio::test]
#[serial]
async fn clear_tx_queue_checks_physical_lock_before_cancelling_held_items() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a and cll_b share one physical channel (same protocol/params).
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

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    let clear_tx_queue_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_CLEAR_TX_QUEUE").await;

    io_ctl_cll(&mut client, cll_b, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");
    start_send_recv(&mut client, cll_b, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // cll_a holds the physical TX queue lock on the channel cll_b shares.
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("lock_resource should succeed");

    let status = io_ctl_cll(&mut client, cll_b, clear_tx_queue_id)
        .await
        .expect_err("PDU_IOCTL_CLEAR_TX_QUEUE should be blocked by cll_a's TX queue lock");
    assert_eq!(status.code(), tonic::Code::ResourceExhausted);

    // The held item must still be intact. PDU_IOCTL_RESUME_TX_QUEUE alone
    // clears only `tx_suspended_by_ioctl` -- cll_a's still-held
    // LOCK_PHYSICAL_TX_QUEUE keeps `tx_suspended_by_lock` true (ADR-123), so
    // the item stays held until cll_a actually releases the lock.
    io_ctl_cll(&mut client, cll_b, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the held item must survive a CLEAR_TX_QUEUE call that was itself rejected by the lock \
         check, and must still be blocked by cll_a's held TX queue lock even after \
         PDU_IOCTL_RESUME_TX_QUEUE"
    );

    // Releasing cll_a's lock resumes dispatch and delivers the held item.
    client
        .unlock_resource(UnlockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("unlock_resource should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the held item must be delivered once cll_a releases the TX queue lock"
    );

    server.shutdown().await;
}

/// ADR-164 Phase 4 backlog fix regression: `PDU_IOCTL_CLEAR_TX_QUEUE` on a
/// channel shared by two CLLs (`SharedChannel::ref_count > 1`) stays a no-op
/// (`Ok`, no native `PassThruIoctl(CLEAR_TX_BUFFER)` call) rather than
/// clearing a sibling CLL's queue -- mirroring `sw_can.rs`'s
/// `sw_can_hs_is_a_no_op_on_a_shared_physical_channel`, the regression test
/// for `ioctl_sw_can_mode`'s own identical TOCTOU fix (this crate's
/// `current_thread` test runtime cannot deterministically land a concurrent
/// `ConnectComLogicalLink` inside the old two-critical-section gap itself,
/// so -- like that precedent -- this pins the observable no-op outcome the
/// now-single, widened `shared_channels` critical section (`rpc_misc.rs`'s
/// `ioctl_clear_tx_queue`) preserves, not the race window directly). Two
/// CLLs created against the same CAN resource with identical params share
/// one physical channel, the same technique
/// `clear_tx_queue_checks_physical_lock_before_cancelling_held_items` above
/// and `reset_on_a_shared_physical_channel_clears_buffers_once_and_keeps_both_clls_connected`
/// below use.
#[tokio::test]
#[serial]
async fn clear_tx_queue_is_a_no_op_on_a_shared_physical_channel() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

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
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "sanity: cll_a and cll_b must actually share one physical channel for this test to \
         exercise the ref_count > 1 gate at all"
    );

    let clear_tx_before = server.backdoor.clear_tx_buffer_count();
    let clear_tx_queue_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_CLEAR_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_a, clear_tx_queue_id)
        .await
        .expect(
            "PDU_IOCTL_CLEAR_TX_QUEUE on a shared channel should be a no-op success, not an \
             error",
        );
    assert_eq!(
        server.backdoor.clear_tx_buffer_count(),
        clear_tx_before,
        "no native CLEAR_TX_BUFFER IOCTL should have reached the mock while the channel is \
         shared (ref_count == 2)"
    );

    let _ = cll_b;
    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 2): `PDU_IOCTL_START_MSG_FILTER`
/// must reject a request containing a duplicate `FilterNumber`, and must reject
/// a `FilterNumber` that already exists in `client_filters` from an earlier
/// call -- in both cases before any hardware call, so no filter is silently
/// orphaned by an overwritten map entry.
#[tokio::test]
#[serial]
async fn start_msg_filter_rejects_duplicate_and_already_installed_filter_numbers() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let one_filter = |filter_number: u32| IoFilter {
        filter_type: PduFilter::PduFltBlock as i32,
        filter_number,
        filter_mask_message: vec![0, 0, 0, 0],
        filter_pattern_message: vec![0, 0, 0, 0],
    };

    let start_filter_count_before = server.backdoor.start_filter_count();

    // Duplicate FilterNumber within the same request: rejected up front, no
    // hardware call made for either entry.
    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(9), one_filter(9)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect_err("duplicate FilterNumber in the same request should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrInvalidParameters as i32
    );
    assert_eq!(
        server.backdoor.start_filter_count(),
        start_filter_count_before,
        "no filter should reach the hardware when the request has a duplicate FilterNumber"
    );

    // Install FilterNumber 9 for real.
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(9)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("the first PDU_IOCTL_START_MSG_FILTER for FilterNumber 9 should succeed");
    let start_filter_count_after_first = server.backdoor.start_filter_count();

    // Reusing FilterNumber 9 without stopping it first must be rejected, and
    // must not touch the hardware or the already-installed filter.
    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(9)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect_err("reusing an already-installed FilterNumber should be rejected");
    assert_eq!(status.code(), tonic::Code::AlreadyExists);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrInvalidParameters as i32
    );
    assert_eq!(
        server.backdoor.start_filter_count(),
        start_filter_count_after_first,
        "no new hardware filter should be installed when FilterNumber 9 is already in use"
    );

    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 3): `PDU_IOCTL_START_MSG_FILTER`
/// must reject a request outright when this CLL's physical channel is shared
/// with another CLL -- a real `PASS_FILTER`/`BLOCK_FILTER` is installed on the
/// shared `channel_id`, not something scoped to one CLL, so it cannot be
/// installed without risking dropping frames a sibling CLL still needs.
#[tokio::test]
#[serial]
async fn start_msg_filter_rejects_on_a_shared_physical_channel() {
    // PDU_FLT_BLOCK, not PASS: PASS/PASS_UUDT are now rejected outright on
    // any channel (round-4 fix), and that check runs before the shared-
    // channel check this test targets -- BLOCK keeps this test exercising
    // the shared-channel rejection specifically.
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a and cll_b share one physical channel (same protocol/params).
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
    assert_eq!(server.backdoor.connect_count(), 1);

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let start_filter_count_before = server.backdoor.start_filter_count();

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_a)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect_err(
            "PDU_IOCTL_START_MSG_FILTER should be rejected when the physical channel is shared",
        );
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(status.message().contains("PDU_ERR_FCT_FAILED"));

    assert_eq!(
        server.backdoor.start_filter_count(),
        start_filter_count_before,
        "no filter should reach the hardware when the request is rejected for a shared channel"
    );

    // Confirm the same rejection applies symmetrically from cll_b's side too.
    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_b)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect_err(
            "PDU_IOCTL_START_MSG_FILTER should be rejected when the physical channel is shared",
        );
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(status.message().contains("PDU_ERR_FCT_FAILED"));
    assert_eq!(
        server.backdoor.start_filter_count(),
        start_filter_count_before,
        "no filter should reach the hardware when the request is rejected for a shared channel"
    );

    server.shutdown().await;
}

/// Conformance-audit fix A2-7 (ADR-129): ISO 22900-2 §9.5.13/§9.4.11.2 d)
/// let a client configure `PDU_IOCTL_START_MSG_FILTER` before `PDUConnect`;
/// the filter becomes active once the CLL reaches `PDU_CLLST_ONLINE`. Asserts
/// the pre-connect call succeeds with no hardware call, and that
/// `ConnectComLogicalLink` installs it once the CLL actually connects.
#[tokio::test]
#[serial]
async fn start_msg_filter_before_connect_is_stored_pending_and_installed_on_connect() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let start_filter_count_before_connect = server.backdoor.start_filter_count();

    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect(
            "PDU_IOCTL_START_MSG_FILTER should succeed before PDUConnect (ISO 22900-2 §9.4.11.2 d)",
        );
    assert_eq!(
        server.backdoor.start_filter_count(),
        start_filter_count_before_connect,
        "a pre-connect filter must not reach the hardware until this CLL actually connects"
    );

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(
        server.backdoor.start_filter_count() - start_filter_count_before_connect,
        4,
        "connecting a CAN link installs this adapter's own 2-variant pass-all filter \
         (install_pass_all_filter, ADR-005/008/039) PLUS the pending client filter, itself \
         installed once per applicable TxFlags/ID-width variant -- 2 + 2 = 4, since raw CAN \
         always connects CAN_ID_BOTH (ADR-065)"
    );

    // FilterNumber 1 must now be tracked as installed (drained from
    // `pending_client_filters` into `client_filters`), not silently dropped.
    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect_err("FilterNumber 1 should now be tracked as already installed after connect");
    assert_eq!(status.code(), tonic::Code::AlreadyExists);

    server.shutdown().await;
}

/// Conformance-audit fix A2-7 (ADR-129): a pre-connect filter must never be
/// silently dropped or force-installed on a channel it does not solely own.
/// `client_filters`'s existing sole-ownership invariant (ADR-082) is extended
/// symmetrically to `pending_client_filters` -- joining an already-shared
/// channel while holding pending filters fails `ConnectComLogicalLink`
/// itself, exactly like the reciprocal "another CLL already installed a
/// filter" case (`start_msg_filter_rejects_on_a_shared_physical_channel`).
#[tokio::test]
#[serial]
async fn connect_fails_when_joining_a_shared_channel_with_pending_filters() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a connects first and owns the physical channel, unfiltered.
    let _cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    // cll_b pre-configures a filter before connecting, then tries to join
    // cll_a's channel (same protocol/baud).
    let cll_b = create_cll(&mut client, j2534_0404::CAN, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let clear_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_CLEAR_MSG_FILTER").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_b)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("pre-connect PDU_IOCTL_START_MSG_FILTER should succeed on cll_b");

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect_err(
            "ConnectComLogicalLink should fail rather than either force-share a filtered CLL's \
             channel or silently drop its pending filters",
        );
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(status.message().contains("PDU_ERR_FCT_FAILED"));
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_b's join attempt must be rejected before any native PassThruConnect for it"
    );

    // The pending filter is retained (not discarded) so the client can clear
    // it and retry -- e.g. by connecting to an unshared channel instead.
    io_ctl_cll(&mut client, cll_b, clear_filter_id)
        .await
        .expect("PDU_IOCTL_CLEAR_MSG_FILTER should succeed pre-connect, with no hardware call");
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect should now succeed once cll_b's pending filters are cleared");

    server.shutdown().await;
}

/// Conformance-audit fix A2-7 (ADR-129): `PDU_IOCTL_STOP_MSG_FILTER` and
/// `PDU_IOCTL_CLEAR_MSG_FILTER` are named alongside `START_MSG_FILTER` in ISO
/// 22900-2 §9.4.11.2 d) as usable before `PDUConnect`. Asserts both remove
/// from `pending_client_filters` directly, with no native call, so a filter
/// undone before connect never reaches the hardware at all.
#[tokio::test]
#[serial]
async fn stop_and_clear_msg_filter_remove_pending_filters_before_connect() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let stop_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_MSG_FILTER").await;
    let clear_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_CLEAR_MSG_FILTER").await;
    let one_filter = |filter_number: u32| IoFilter {
        filter_type: PduFilter::PduFltBlock as i32,
        filter_number,
        filter_mask_message: vec![0, 0, 0, 0],
        filter_pattern_message: vec![0, 0, 0, 0],
    };

    // STOP_MSG_FILTER removes a single pending FilterNumber.
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(1)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("pre-connect PDU_IOCTL_START_MSG_FILTER should succeed");

    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                stop_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::Unum32Value(1)),
            }),
            has_output: false,
        })
        .await
        .expect("pre-connect PDU_IOCTL_STOP_MSG_FILTER should succeed, with no native call");

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                stop_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::Unum32Value(1)),
            }),
            has_output: false,
        })
        .await
        .expect_err("FilterNumber 1 should no longer be tracked after the pre-connect stop");
    // A2-19 (ISO 22900-2:2009 Table 55): an unrecognized FilterNumber is
    // PDU_ERR_INVALID_PARAMETERS, not PDU_ERR_INVALID_HANDLE.
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrInvalidParameters as i32
    );

    // CLEAR_MSG_FILTER empties every remaining pending FilterNumber.
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(2), one_filter(3)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("pre-connect PDU_IOCTL_START_MSG_FILTER should succeed");

    io_ctl_cll(&mut client, cll_handle, clear_filter_id)
        .await
        .expect("pre-connect PDU_IOCTL_CLEAR_MSG_FILTER should succeed, with no native call");

    let start_filter_count_before_connect = server.backdoor.start_filter_count();
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");
    assert_eq!(
        server.backdoor.start_filter_count() - start_filter_count_before_connect,
        2,
        "no CLIENT filter should install at connect time -- every pending FilterNumber was \
         stopped or cleared before PDUConnect -- but connecting a CAN link still installs this \
         adapter's own 2-variant pass-all filter regardless (install_pass_all_filter, \
         ADR-005/008/039)"
    );

    server.shutdown().await;
}

/// A2-19 (ISO 22900-2:2009 Table 55): once a CLL is connected,
/// `PDU_IOCTL_STOP_MSG_FILTER` for a `FilterNumber` that was never installed
/// in `client_filters` must report `PDU_ERR_INVALID_PARAMETERS`, not
/// `PDU_ERR_INVALID_HANDLE` (reserved for an invalid ComLogicalLink handle).
/// Covers the connected-CLL miss branch; the pre-connect
/// `pending_client_filters` miss branch is covered by
/// `stop_and_clear_msg_filter_remove_pending_filters_before_connect` above.
#[tokio::test]
#[serial]
async fn stop_msg_filter_reports_invalid_parameters_for_unrecognized_filter_number_when_connected()
{
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let stop_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_MSG_FILTER").await;

    // FilterNumber 42 was never installed via PDU_IOCTL_START_MSG_FILTER.
    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                stop_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::Unum32Value(42)),
            }),
            has_output: false,
        })
        .await
        .expect_err("stopping a never-installed FilterNumber should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrInvalidParameters as i32,
        "an unrecognized FilterNumber on a connected CLL must report \
         PDU_ERR_INVALID_PARAMETERS, not PDU_ERR_INVALID_HANDLE"
    );

    server.shutdown().await;
}

/// Conformance-audit fix A2-7 (ADR-129), adversarial-review follow-up: the
/// already-installed-or-pending duplicate `FilterNumber` rejection
/// (`rpc_misc.rs`) must reject a second pre-connect `START_MSG_FILTER` that
/// reuses a `FilterNumber` already sitting in `pending_client_filters` --
/// not just one already in `client_filters` (the only case the pre-existing
/// `start_msg_filter_rejects_duplicate_and_already_installed_filter_numbers`
/// test covers, since that CLL is connected throughout).
#[tokio::test]
#[serial]
async fn start_msg_filter_before_connect_rejects_a_filter_number_already_pending() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let one_filter = |filter_number: u32| IoFilter {
        filter_type: PduFilter::PduFltBlock as i32,
        filter_number,
        filter_mask_message: vec![0, 0, 0, 0],
        filter_pattern_message: vec![0, 0, 0, 0],
    };

    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(5)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect(
            "the first pre-connect PDU_IOCTL_START_MSG_FILTER for FilterNumber 5 should succeed",
        );

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(5)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect_err(
            "reusing a FilterNumber already pending (never installed, since this CLL is still \
             unconnected) should be rejected",
        );
    assert_eq!(status.code(), tonic::Code::AlreadyExists);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrInvalidParameters as i32
    );

    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 3): the legacy raw
/// `CLEAR_MSG_FILTERS` IoCtl wipes every hardware filter on the channel,
/// including client filters installed via `PDU_IOCTL_START_MSG_FILTER` by
/// every CLL sharing it -- their `client_filters` entries must be purged too,
/// or reusing the same `FilterNumber` afterward is wrongly rejected as
/// already-installed even though the underlying hardware filter is gone.
#[tokio::test]
#[serial]
async fn legacy_clear_msg_filters_purges_client_filters_so_filter_number_reuse_succeeds() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let one_filter = |filter_number: u32| IoFilter {
        filter_type: PduFilter::PduFltBlock as i32,
        filter_number,
        filter_mask_message: vec![0, 0, 0, 0],
        filter_pattern_message: vec![0, 0, 0, 0],
    };

    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(5)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("the first PDU_IOCTL_START_MSG_FILTER for FilterNumber 5 should succeed");

    // The legacy raw CLEAR_MSG_FILTERS ioctl (distinct from
    // PDU_IOCTL_CLEAR_MSG_FILTER) wipes every hardware filter on the channel,
    // including the one just installed above.
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                j2534_0404::CLEAR_MSG_FILTERS,
            )),
            input_data: None,
            has_output: false,
        })
        .await
        .expect("io_ctl(CLEAR_MSG_FILTERS) should succeed");

    // Reusing FilterNumber 5 must now succeed: the stale client_filters entry
    // must have been purged, not left blocking reuse with an already-exists
    // error.
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(5)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect(
            "reusing FilterNumber 5 after the legacy CLEAR_MSG_FILTERS should succeed, proving \
             the stale client_filters entry was purged",
        );

    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 5): `PDU_IOCTL_CLEAR_MSG_FILTER`
/// must stop every filter tracked in this CLL's `client_filters` map, and must
/// remove each `FilterNumber` from the map once its hardware filter actually
/// stopped -- otherwise a `FilterNumber` would be wrongly rejected as
/// already-installed on reuse even though `PassThruStopMsgFilter` never failed
/// for it. This only covers the success path; the keep-on-failure branch
/// (and its ADR-114 `PDU_ERR_FCT_FAILED` reporting) is covered by
/// `clear_msg_filter_reports_fct_failed_and_keeps_filters_tracked_on_native_stop_failure`
/// below, via `set_stop_filter_error`.
#[tokio::test]
#[serial]
async fn clear_msg_filter_stops_all_client_filters_and_empties_the_map() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let clear_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_CLEAR_MSG_FILTER").await;
    let one_filter = |filter_number: u32| IoFilter {
        filter_type: PduFilter::PduFltBlock as i32,
        filter_number,
        filter_mask_message: vec![0, 0, 0, 0],
        filter_pattern_message: vec![0, 0, 0, 0],
    };

    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(1), one_filter(2)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("installing FilterNumbers 1 and 2 should succeed");

    let stop_filter_count_before = server.backdoor.stop_filter_count();

    io_ctl_cll(&mut client, cll_handle, clear_filter_id)
        .await
        .expect("PDU_IOCTL_CLEAR_MSG_FILTER should succeed");

    assert_eq!(
        server.backdoor.stop_filter_count() - stop_filter_count_before,
        4,
        "PDU_IOCTL_CLEAR_MSG_FILTER should stop both installed filters -- each installed as 2 \
         hardware filters (TxFlags 0 and TX_EXTENDED_ID) because raw CAN always connects \
         CAN_ID_BOTH (ADR-065)"
    );

    // Reusing FilterNumbers 1 and 2 must now succeed: client_filters must be
    // empty, not still tracking either FilterNumber as already-installed.
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(1), one_filter(2)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect(
            "reusing FilterNumbers 1 and 2 after PDU_IOCTL_CLEAR_MSG_FILTER should succeed, \
             proving client_filters ended up empty",
        );

    server.shutdown().await;
}

/// ADR-114 (fixes conformance-audit item B21): `PDU_IOCTL_STOP_MSG_FILTER`
/// must report `PDU_ERR_FCT_FAILED` when the underlying native
/// `PassThruStopMsgFilter` call fails, instead of silently returning
/// `PDU_STATUS_NOERROR` -- while still keeping the `FilterNumber` tracked in
/// `client_filters` for retry (ADR-079's best-effort philosophy, unchanged).
#[tokio::test]
#[serial]
async fn stop_msg_filter_reports_fct_failed_and_keeps_filter_tracked_on_native_stop_failure() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let stop_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_MSG_FILTER").await;
    let one_filter = |filter_number: u32| IoFilter {
        filter_type: PduFilter::PduFltBlock as i32,
        filter_number,
        filter_mask_message: vec![0, 0, 0, 0],
        filter_pattern_message: vec![0, 0, 0, 0],
    };

    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(7)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("installing FilterNumber 7 should succeed");

    server.backdoor.set_stop_filter_error(Some(0x99));

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                stop_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::Unum32Value(7)),
            }),
            has_output: false,
        })
        .await
        .expect_err(
            "PDU_IOCTL_STOP_MSG_FILTER should report PDU_ERR_FCT_FAILED when the native stop \
             call fails",
        );
    assert_eq!(status.code(), tonic::Code::Internal);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrFctFailed as i32
    );

    // FilterNumber 7 must still be tracked (as already-installed), proving
    // the failed stop was not silently forgotten.
    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(7)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect_err(
            "FilterNumber 7 should still be tracked as already-installed after the failed stop",
        );
    assert_eq!(status.code(), tonic::Code::AlreadyExists);

    // Clearing the override and retrying STOP_MSG_FILTER must now succeed --
    // the client's documented recovery path.
    server.backdoor.set_stop_filter_error(None);
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                stop_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::Unum32Value(7)),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_STOP_MSG_FILTER should succeed once the override is cleared");

    // FilterNumber 7 must now be reusable, proving the retry actually cleared
    // it from client_filters.
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(7)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("reusing FilterNumber 7 after the successful retry should succeed");

    server.shutdown().await;
}

/// ADR-114 (fixes conformance-audit item B21): `PDU_IOCTL_CLEAR_MSG_FILTER`
/// must report `PDU_ERR_FCT_FAILED` when any underlying native
/// `PassThruStopMsgFilter` call fails, instead of silently returning
/// `PDU_STATUS_NOERROR` -- while still keeping the affected `FilterNumber`s
/// tracked in `client_filters` for retry. The mock's stop-filter-error
/// override (`set_stop_filter_error`) is a blanket per-call override, not
/// targetable to a specific filter id, so this exercises the "every
/// installed filter fails to stop" case rather than a partial failure.
#[tokio::test]
#[serial]
async fn clear_msg_filter_reports_fct_failed_and_keeps_filters_tracked_on_native_stop_failure() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let clear_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_CLEAR_MSG_FILTER").await;
    let one_filter = |filter_number: u32| IoFilter {
        filter_type: PduFilter::PduFltBlock as i32,
        filter_number,
        filter_mask_message: vec![0, 0, 0, 0],
        filter_pattern_message: vec![0, 0, 0, 0],
    };

    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(1), one_filter(2)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("installing FilterNumbers 1 and 2 should succeed");

    server.backdoor.set_stop_filter_error(Some(0x99));

    let status = io_ctl_cll(&mut client, cll_handle, clear_filter_id)
        .await
        .expect_err(
            "PDU_IOCTL_CLEAR_MSG_FILTER should report PDU_ERR_FCT_FAILED when every native stop \
             call fails",
        );
    assert_eq!(status.code(), tonic::Code::Internal);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrFctFailed as i32
    );
    // The message must name the specific failed FilterNumbers (sorted), not
    // just a generic "some failed" -- pins the sorted_failed-before-drain
    // ordering `ioctl_clear_msg_filter` relies on.
    assert!(
        status.message().contains("[1, 2]"),
        "message should name the failed FilterNumbers: {}",
        status.message()
    );

    // Both FilterNumbers must still be tracked as already-installed, proving
    // the failed stops were not silently forgotten.
    for filter_number in [1, 2] {
        let status = client
            .io_ctl(IoCtlRequest {
                handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
                io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                    start_filter_id,
                )),
                input_data: Some(DataItem {
                    data: Some(data_item::Data::FilterData(IoFilterList {
                        filters: vec![one_filter(filter_number)],
                    })),
                }),
                has_output: false,
            })
            .await
            .expect_err(&format!(
                "FilterNumber {filter_number} should still be tracked as already-installed \
                 after the failed CLEAR_MSG_FILTER"
            ));
        assert_eq!(status.code(), tonic::Code::AlreadyExists);
    }

    // Clearing the override and retrying CLEAR_MSG_FILTER must now succeed,
    // and client_filters must end up empty.
    server.backdoor.set_stop_filter_error(None);
    io_ctl_cll(&mut client, cll_handle, clear_filter_id)
        .await
        .expect("PDU_IOCTL_CLEAR_MSG_FILTER should succeed once the override is cleared");

    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(1), one_filter(2)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect(
            "reusing FilterNumbers 1 and 2 after the successful retry should succeed, proving \
             client_filters ended up empty",
        );

    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 3): `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`
/// must trim an already-over-cap `rx_buf` down to the new, lower `QueueSize`
/// immediately -- not just gate future inserts, which would otherwise leave
/// the buffer over-cap indefinitely under continued traffic (and, in
/// `PDU_QUE_LIMITED` mode, block all new frames until the client drains it
/// back under the cap on its own).
///
/// A2-5 fix (ADR-126): `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES` is now rejected
/// with `PDU_ERR_CLL_CONNECTED` on a connected CLL, so a live shrink is no
/// longer reachable -- the trim logic is instead exercised via a
/// disconnect-then-shrink flow: connect, receive traffic that accumulates
/// past the new cap in `rx_buf`, disconnect (which does not clear `rx_buf`),
/// then shrink the queue while offline. The core assertion (the trim happens
/// immediately, not lazily on the next push) is unchanged.
#[tokio::test]
#[serial]
async fn set_event_queue_properties_trims_rx_buf_immediately_when_lowering_the_cap() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps every injected
    // frame below delivered under the new unbound-discard model (see
    // `arm_receive_only_monitor`'s own doc comment).
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // No UniqueRespIdTable is configured, so routing delivers every injected
    // frame unconditionally (see rx_header_split.rs). Inject 5 distinguishable
    // frames -- more than the cap this test is about to set.
    for i in 0..5u8 {
        let mut frame = 0x100_u32.to_be_bytes().to_vec();
        frame.push(i);
        server
            .backdoor
            .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);
    }

    // Give the poll task ample time to dequeue and push all 5 frames into
    // rx_buf before the cap is lowered.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // A2-5 fix (ADR-126): disconnect before reconfiguring the event queue --
    // disconnect clears `connected`/`channel_id` but not `rx_buf` (see
    // `rpc_link.rs`'s `DisconnectComLogicalLink`), so the over-cap backlog
    // accumulated above is still present to be trimmed.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    let set_queue_props_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_queue_props_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::EventQueueProperty(IoEventQueueProperty {
                    queue_size: 2,
                    queue_mode: PduQueueMode::PduQueLimited as i32,
                })),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES should succeed");

    // Drain rx_buf via GetEventItem and record what remains -- the trim must
    // already have happened, not merely be pending on future frames.
    //
    // ADR-105 P2 follow-up: `ConnectComLogicalLink`/`DisconnectComLogicalLink`
    // now each enqueue a `CllStatus` transition into the same `rx_buf` FIFO
    // (`send_cll_status`, no longer subscription-only), so the pre-trim
    // backlog here already includes `CllStatus(Online)` (from
    // `create_and_connect_cll`'s own connect), the 5 injected frames, and
    // `CllStatus(Offline)` (from the `disconnect_com_logical_link` call
    // above). `send_cop_status` queueing (this PR) adds one more: the
    // `arm_receive_only_monitor` registrant is IS-CYCLIC and never finishes
    // on its own, so `disconnect_com_logical_link`'s own `cancel_link_cops`
    // call cancels it, queueing `CopStatus(Cancelled)` -- emitted before
    // `CllStatus(Offline)` per that handler's own documented ordering ("Done
    // before PduCllstOffline so events arrive in order", `rpc_link.rs`). Full
    // pre-trim FIFO order: `[CllOnline, Frame(0..4), CopCancelled,
    // CllOffline]` (8 entries) -- all counted like any other entry against
    // the same cap/eviction policy this test is about ("Do not invent new
    // eviction logic" -- ADR-105 P2 follow-up).
    #[derive(Debug, PartialEq, Eq)]
    enum Remaining {
        Frame(Vec<u8>),
        CllOnline,
        CllOffline,
        CopCancelled,
    }
    let mut remaining = Vec::new();
    loop {
        let event_item = client
            .get_event_item(GetEventItemRequest {
                handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("get_event_item should succeed")
            .into_inner()
            .event_item;
        let Some(event_item) = event_item else {
            break;
        };
        match event_item.data {
            Some(event_item::Data::ResultData(result)) => {
                remaining.push(Remaining::Frame(result.data_bytes))
            }
            Some(event_item::Data::CllStatus(status))
                if status == PduComLogicalLinkStatus::PduCllstOnline as i32 =>
            {
                remaining.push(Remaining::CllOnline)
            }
            Some(event_item::Data::CllStatus(status))
                if status == PduComLogicalLinkStatus::PduCllstOffline as i32 =>
            {
                remaining.push(Remaining::CllOffline)
            }
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32 =>
            {
                remaining.push(Remaining::CopCancelled)
            }
            other => panic!("unexpected event_item data: {other:?}"),
        }
    }

    // Pre-trim FIFO order: [CllOnline, Frame(0..4), CopCancelled, CllOffline]
    // (8 entries). The trim pops from the front (oldest first) down to the
    // new cap (2), so the two survivors are the monitor's own Cancelled
    // transition (queued at disconnect, ahead of Offline) and the trailing
    // Offline transition itself.
    assert_eq!(
        remaining,
        vec![Remaining::CopCancelled, Remaining::CllOffline],
        "rx_buf should already be trimmed to the new cap (2), not left over-cap"
    );

    server.shutdown().await;
}

/// A2-5 fix (ADR-126): `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES` is a pre-Connect-only
/// IOCTL per ISO 22900-2 §9.5.16 (it is only valid before PDUConnect; on an
/// already-connected ComLogicalLink the call fails with
/// `PDU_ERR_CLL_CONNECTED`). Calling it on a connected CLL must be rejected
/// with `PDU_ERR_CLL_CONNECTED`, not applied.
#[tokio::test]
#[serial]
async fn set_event_queue_properties_rejects_when_connected() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;

    let set_queue_props_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES").await;
    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_queue_props_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::EventQueueProperty(IoEventQueueProperty {
                    queue_size: 2,
                    queue_mode: PduQueueMode::PduQueLimited as i32,
                })),
            }),
            has_output: false,
        })
        .await
        .expect_err("PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES should be rejected on a connected CLL");
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrCllConnected as i32
    );

    server.shutdown().await;
}

/// Reads the index byte (last byte of `data_bytes`) off one `SubscribeEvent`
/// notification, panicking on anything but a `ResultData` item -- shared by
/// the ADR-115 round-3-correction tests below that don't expect a `Lost`
/// notification to appear (the scenarios that DO expect one, e.g.
/// `backlog_evicted_then_subscribe_emits_exactly_one_lost_then_drains_fifo`,
/// classify each notification themselves instead of using this helper).
fn expect_frame_index_byte(notification: vci_service_interface::EventNotification) -> u8 {
    match notification.event_data {
        Some(event_notification::EventData::Item(item)) => match item.data {
            Some(event_item::Data::ResultData(result)) => *result
                .data_bytes
                .last()
                .expect("frame should carry its index byte"),
            other => panic!("unexpected event_item data: {other:?}"),
        },
        other => panic!("unexpected notification: {other:?}"),
    }
}

/// ADR-115 round-3 correction (PR #122 round 3, on top of the round-2
/// regression fix for the original A2-6 cut): `deliver_or_enqueue` now
/// always pushes through the cap/mode-enforcing `push_cll_event` first, then
/// -- in the same critical section -- opportunistically drains `rx_buf`'s
/// full current contents out live to a live subscriber. `queue_size` (2) is
/// set well below the 5 frames injected, but since the subscriber is live
/// and keeping up, each push's immediate full drain means `rx_buf` never
/// actually holds more than 1 item at a time -- it oscillates empty ->
/// one-item -> empty every cycle, so the cap is never crossed and nothing is
/// ever genuinely dropped: no `PDU_EVT_DATA_LOST` notification is sent (this
/// is the CORRECT reason now, not round 2's "Lost is structurally
/// unreachable for any live subscriber" bug), all 5 frames arrive live in
/// order, and `GetEventItem` finds nothing left to drain afterward.
#[tokio::test]
#[serial]
async fn subscribe_only_limited_mode_delivers_every_frame_live_with_no_loss() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // A2-5 fix (ADR-126): PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES can now only be
    // used prior to PDUConnect, so the queue is configured on a not-yet-connected
    // CLL before Connect is issued.
    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;
    for &(com_param_id, value) in &[
        (j2534_0404::DATA_RATE, 500_000),
        (CP_REQUEST_ADDR_MODE, 2),
        (CP_CAN_FUNC_REQ_ID, 0x7DF),
    ] {
        set_com_param_unum32(&mut client, cll_handle, com_param_id, value).await;
    }

    let set_queue_props_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_queue_props_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::EventQueueProperty(IoEventQueueProperty {
                    queue_size: 2,
                    queue_mode: PduQueueMode::PduQueLimited as i32,
                })),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES should succeed");

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // ADR-100 Decision §5 (S8): a receive-only monitor keeps every injected
    // frame deliverable under the new unbound-discard model (see
    // `arm_receive_only_monitor`'s own doc comment). Armed AFTER the
    // subscription attaches (P2 backlog follow-up, `docs/implementation-notes.md`):
    // `send_cop_status` now also enqueues into this same `rx_buf` FIFO (no
    // longer subscription-only), so arming it before `subscribe_event` would
    // leave its own `CopStatus(Executing)` transition sitting in the cap-2
    // backlog alongside `CllStatus(Online)` -- already filling the cap
    // before the first frame is even pushed, discarding frame 0 outright and
    // defeating this test's own "no loss" premise. Arming it after
    // subscribing instead means its `Executing` transition is pushed (and
    // immediately drained live, in the same `deliver_or_enqueue` call, right
    // behind the pre-existing `CllStatus(Online)` backlog entry) BEFORE any
    // frame is injected, so `rx_buf` is empty again by the time frame 0
    // arrives -- preserving the intended cap-never-crossed, no-loss
    // behavior.
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // Inject cap (2) + 3 = 5 frames -- well past the cap this queue would
    // otherwise drop entries at, with the subscriber live the entire time.
    for i in 0..5u8 {
        let mut frame = 0x100_u32.to_be_bytes().to_vec();
        frame.push(i);
        server
            .backdoor
            .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);
    }

    // ADR-105 P2 follow-up: `ConnectComLogicalLink` (above, before this
    // subscription attached) already enqueued a `CllStatus(Online)` entry
    // into `rx_buf` via `send_cll_status` -- no longer subscription-only,
    // and `arm_receive_only_monitor` (above, after subscribing) pushed its
    // own `CopStatus(Executing)` entry right behind it, in the same live
    // drain. Attaching a subscriber does not itself drain existing backlog
    // (only a subsequent push does, via `deliver_or_enqueue`), so both
    // entries deterministically drain out together as the first two live
    // notifications, ahead of any frame -- read and discarded here so
    // neither is mistaken for frame data below.
    let leading = tokio::time::timeout(tokio::time::Duration::from_millis(2000), events.message())
        .await
        .expect("should not time out waiting for the leading CllStatus notification")
        .expect("stream should not error")
        .expect("stream should not end");
    assert!(
        matches!(
            leading.event_data,
            Some(event_notification::EventData::Item(item))
                if matches!(
                    item.data,
                    Some(event_item::Data::CllStatus(status))
                        if status == PduComLogicalLinkStatus::PduCllstOnline as i32
                )
        ),
        "the very first live notification should be the CllStatus(Online) transition \
         enqueued at ConnectComLogicalLink, drained ahead of any frame data"
    );
    let leading_cop_status =
        tokio::time::timeout(tokio::time::Duration::from_millis(2000), events.message())
            .await
            .expect("should not time out waiting for the leading CopStatus notification")
            .expect("stream should not error")
            .expect("stream should not end");
    assert!(
        matches!(
            leading_cop_status.event_data,
            Some(event_notification::EventData::Item(item))
                if matches!(
                    item.data,
                    Some(event_item::Data::CopStatus(status))
                        if status == PduComPrimitiveStatus::PduCopstExecuting as i32
                )
        ),
        "the second live notification should be the receive-only monitor's own \
         CopStatus(Executing) transition, drained right behind CllStatus(Online) and still \
         ahead of any frame data"
    );

    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(2000);
    while seen.len() < 5 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for all 5 frames live; seen so far: {seen:?}"
        );
        let notification = tokio::time::timeout(remaining, events.message())
            .await
            .expect("should not time out waiting for a notification")
            .expect("stream should not error")
            .expect("stream should not end");
        seen.push(expect_frame_index_byte(notification));
    }

    assert_eq!(
        seen,
        vec![0, 1, 2, 3, 4],
        "all 5 frames should be delivered live, in order, with no Lost notifications"
    );

    // rx_buf must be empty -- every item was delivered live, none written to
    // it (ADR-115 single-consumer correction).
    let event_item = client
        .get_event_item(GetEventItemRequest {
            handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("get_event_item should succeed")
        .into_inner()
        .event_item;
    assert!(
        event_item.is_none(),
        "rx_buf should be empty -- every frame was delivered live, none buffered: {event_item:?}"
    );

    drop(events);
    server.shutdown().await;
}

/// No `SubscribeEvent` subscriber attached at all: unaffected by the
/// ADR-115 single-consumer correction -- `push_cll_event`'s pre-existing
/// `DiscardNewest`/`Limited` semantics apply exactly as before, since there
/// is no live channel for `deliver_or_enqueue` to drain into.
///
/// ADR-105 P2 follow-up: `ConnectComLogicalLink` now also enqueues a
/// `CllStatus(Online)` entry (`send_cll_status`, no longer subscription-only)
/// ahead of any frame -- it fills the first of the cap's 2 slots.
///
/// P2 backlog follow-up (`docs/implementation-notes.md`): `send_cop_status`
/// now also enqueues into this same `rx_buf` FIFO (no longer
/// subscription-only), so `arm_receive_only_monitor`'s own
/// `CopStatus(Executing)` transition -- queued right behind `CllStatus(Online)`,
/// since it is armed before this test's own frame injection -- fills the
/// *second* of the cap's 2 slots. Both slots are full before frame 0 is even
/// pushed, so with no live subscriber to drain into, `DiscardNewest` drops
/// every one of the 5 injected frames outright (index bytes 0-4 all
/// discarded), leaving `rx_buf` at exactly `[CllStatus(Online),
/// CopStatus(Executing)]`.
#[tokio::test]
#[serial]
async fn no_subscriber_limited_mode_drops_per_push_cll_event_semantics() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // A2-5 fix (ADR-126): PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES can now only be
    // used prior to PDUConnect, so the queue is configured on a not-yet-connected
    // CLL before Connect is issued.
    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;
    for &(com_param_id, value) in &[
        (j2534_0404::DATA_RATE, 500_000),
        (CP_REQUEST_ADDR_MODE, 2),
        (CP_CAN_FUNC_REQ_ID, 0x7DF),
    ] {
        set_com_param_unum32(&mut client, cll_handle, com_param_id, value).await;
    }

    let set_queue_props_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_queue_props_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::EventQueueProperty(IoEventQueueProperty {
                    queue_size: 2,
                    queue_mode: PduQueueMode::PduQueLimited as i32,
                })),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES should succeed");

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");
    arm_receive_only_monitor(&mut client, cll_handle).await;

    for i in 0..5u8 {
        let mut frame = 0x100_u32.to_be_bytes().to_vec();
        frame.push(i);
        server
            .backdoor
            .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);
    }

    // Give the poll task ample time to process all 5 frames.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    #[derive(Debug, PartialEq, Eq)]
    enum Remaining {
        Frame(u8),
        CllOnline,
        CopExecuting,
    }
    let mut remaining = Vec::new();
    loop {
        let event_item = client
            .get_event_item(GetEventItemRequest {
                handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("get_event_item should succeed")
            .into_inner()
            .event_item;
        let Some(event_item) = event_item else {
            break;
        };
        match event_item.data {
            Some(event_item::Data::ResultData(result)) => remaining.push(Remaining::Frame(
                *result
                    .data_bytes
                    .last()
                    .expect("frame should carry its index byte"),
            )),
            Some(event_item::Data::CllStatus(status))
                if status == PduComLogicalLinkStatus::PduCllstOnline as i32 =>
            {
                remaining.push(Remaining::CllOnline)
            }
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstExecuting as i32 =>
            {
                remaining.push(Remaining::CopExecuting)
            }
            other => panic!("unexpected event_item data: {other:?}"),
        }
    }

    assert_eq!(
        remaining,
        vec![Remaining::CllOnline, Remaining::CopExecuting],
        "DiscardNewest with no subscriber keeps no frames at all: CllStatus(Online) and the \
         receive-only monitor's own CopStatus(Executing) already filled both of the cap's (2) \
         slots before any frame was pushed"
    );

    server.shutdown().await;
}

/// Backlog accumulated in `rx_buf` before a subscriber attaches drains out
/// live, in FIFO order, ahead of a frame injected after the subscriber
/// attaches (ADR-115 single-consumer correction: `deliver_or_enqueue`'s
/// drain-then-send order).
///
/// ADR-105 P2 follow-up: `create_and_connect_cll` below also enqueues a
/// `CllStatus(Online)` entry (`send_cll_status`, no longer
/// subscription-only) ahead of the 2-frame backlog, so it is the actual
/// FIFO head and drains out first, before frame 0.
///
/// P2 backlog follow-up (`docs/implementation-notes.md`): `send_cop_status`
/// now also enqueues into this same `rx_buf` FIFO (no longer
/// subscription-only), and `arm_receive_only_monitor` must be armed before
/// the 2-frame backlog is injected (so it can actually observe those
/// frames), so its own `CopStatus(Executing)` transition lands right behind
/// `CllStatus(Online)` and ahead of frame 0 too.
#[tokio::test]
#[serial]
async fn backlog_then_subscribe_drains_fifo_before_the_new_frame() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // No subscriber yet -- these 2 frames accumulate as backlog in rx_buf
    // (default queue mode/cap, well above 2).
    for i in 0..2u8 {
        let mut frame = 0x100_u32.to_be_bytes().to_vec();
        frame.push(i);
        server
            .backdoor
            .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Inject one more frame now that a subscriber is live -- the poll
    // task's next fan-out for this frame should drain the 2-frame backlog
    // first (FIFO), then this frame.
    let mut frame = 0x100_u32.to_be_bytes().to_vec();
    frame.push(2);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);

    // The FIFO head is actually `CllStatus(Online)` (enqueued at
    // `create_and_connect_cll`'s own connect, before any frame), and
    // attaching a subscriber does not itself drain existing backlog -- only
    // this next push does, via `deliver_or_enqueue` -- so it deterministically
    // drains out first, ahead of frame 0.
    let leading = tokio::time::timeout(tokio::time::Duration::from_millis(2000), events.message())
        .await
        .expect("should not time out waiting for the leading CllStatus notification")
        .expect("stream should not error")
        .expect("stream should not end");
    assert!(
        matches!(
            leading.event_data,
            Some(event_notification::EventData::Item(item))
                if matches!(
                    item.data,
                    Some(event_item::Data::CllStatus(status))
                        if status == PduComLogicalLinkStatus::PduCllstOnline as i32
                )
        ),
        "the FIFO head should be the CllStatus(Online) transition enqueued at connect, \
         drained ahead of the frame backlog"
    );

    // Right behind it: the receive-only monitor's own CopStatus(Executing)
    // transition, queued when it was armed (before the 2-frame backlog),
    // still ahead of frame 0.
    let leading_cop_status =
        tokio::time::timeout(tokio::time::Duration::from_millis(2000), events.message())
            .await
            .expect("should not time out waiting for the leading CopStatus notification")
            .expect("stream should not error")
            .expect("stream should not end");
    assert!(
        matches!(
            leading_cop_status.event_data,
            Some(event_notification::EventData::Item(item))
                if matches!(
                    item.data,
                    Some(event_item::Data::CopStatus(status))
                        if status == PduComPrimitiveStatus::PduCopstExecuting as i32
                )
        ),
        "the second FIFO entry should be the receive-only monitor's own CopStatus(Executing) \
         transition, drained right behind CllStatus(Online) and still ahead of the frame \
         backlog"
    );

    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(2000);
    while seen.len() < 3 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for backlog + new frame; seen so far: {seen:?}"
        );
        let notification = tokio::time::timeout(remaining, events.message())
            .await
            .expect("should not time out waiting for a notification")
            .expect("stream should not error")
            .expect("stream should not end");
        seen.push(expect_frame_index_byte(notification));
    }

    assert_eq!(
        seen,
        vec![0, 1, 2],
        "backlog (0, 1) must drain live in FIFO order before the new frame (2)"
    );

    let event_item = client
        .get_event_item(GetEventItemRequest {
            handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("get_event_item should succeed")
        .into_inner()
        .event_item;
    assert!(
        event_item.is_none(),
        "rx_buf should be empty after the backlog fully drained live: {event_item:?}"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-115 round-3 correction, the transition case: a backlog built up with
/// NO subscriber attached, then a subscriber attaches while that backlog is
/// still at cap. `push_cll_event` now runs unconditionally on every push, so
/// the very next push after the subscriber attaches can genuinely cross the
/// cap and report a drop -- observable this time, since a live channel now
/// exists to carry `Lost`. Uses `OverwriteOldest` (`PDU_QUE_CIRCULAR`)
/// specifically to also exercise Round 1's original self-contradiction
/// concern: the evicted item and its replacement must never both appear to
/// be "the same" delivery -- `Lost` corresponds to the genuinely-evicted
/// older item, and the new item that replaced it is delivered normally via
/// the same drain loop, not conflated with the `Lost` signal.
///
/// Sequence (cap 2, `OverwriteOldest`):
/// - No subscriber: frames 0, 1 fill the cap; frame 2 evicts 0 (buf ->
///   [1, 2]); frame 3 evicts 1 (buf -> [2, 3]). Neither eviction has a live
///   channel to report `Lost` to.
/// - Subscriber attaches. Frame 4 is pushed: buf is still at cap ([2, 3]),
///   so this push evicts 2 (buf -> [3, 4]) and reports `Evicted` --
///   this time observable. Exactly one `Lost` fires, then the full
///   surviving buffer ([3, 4]) drains live, FIFO, in the same critical
///   section.
/// - Frame 5, pushed once the backlog is empty, delivers clean: no `Lost`,
///   immediate live delivery -- confirming the transition's one drop does
///   not recur on subsequent traffic.
#[tokio::test]
#[serial]
async fn backlog_evicted_then_subscribe_emits_exactly_one_lost_then_drains_fifo() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // A2-5 fix (ADR-126): PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES can now only be
    // used prior to PDUConnect, so the queue is configured on a not-yet-connected
    // CLL before Connect is issued.
    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;
    for &(com_param_id, value) in &[
        (j2534_0404::DATA_RATE, 500_000),
        (CP_REQUEST_ADDR_MODE, 2),
        (CP_CAN_FUNC_REQ_ID, 0x7DF),
    ] {
        set_com_param_unum32(&mut client, cll_handle, com_param_id, value).await;
    }

    let set_queue_props_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_queue_props_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::EventQueueProperty(IoEventQueueProperty {
                    queue_size: 2,
                    queue_mode: PduQueueMode::PduQueCircular as i32,
                })),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES should succeed");

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // No subscriber yet: `connect_com_logical_link` above already pushed a
    // leading `CllStatus(Online)` entry into the same cap-2 OverwriteOldest
    // queue (ADR-105 P2 follow-up), then frames 0-3 push through it. That
    // leading entry is evicted by frame 1 before frame 2 arrives, so the
    // final-state arithmetic below is unaffected -- it converges on "last 2
    // items pushed" regardless: 0 and 1 fill the cap (evicting Online); 2
    // evicts 0 (buf -> [1, 2]); 3 evicts 1 (buf -> [2, 3]). No live channel
    // exists to report any of these evictions.
    for i in 0..4u8 {
        let mut frame = 0x100_u32.to_be_bytes().to_vec();
        frame.push(i);
        server
            .backdoor
            .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Push frame 4 now that a subscriber is live: rx_buf is still at cap
    // ([2, 3]), so this push evicts 2 (buf -> [3, 4]) and reports Evicted --
    // now observable.
    let mut frame4 = 0x100_u32.to_be_bytes().to_vec();
    frame4.push(4);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame4, j2534_0404::CAN);

    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Lost,
        Frame(u8),
    }

    async fn next_notification(
        events: &mut tonic::codec::Streaming<vci_service_interface::EventNotification>,
    ) -> Seen {
        let notification =
            tokio::time::timeout(tokio::time::Duration::from_millis(2000), events.message())
                .await
                .expect("should not time out waiting for a notification")
                .expect("stream should not error")
                .expect("stream should not end");
        match notification.event_data {
            Some(event_notification::EventData::Lost(_)) => Seen::Lost,
            Some(event_notification::EventData::Item(item)) => match item.data {
                Some(event_item::Data::ResultData(result)) => Seen::Frame(
                    *result
                        .data_bytes
                        .last()
                        .expect("frame should carry its index byte"),
                ),
                other => panic!("unexpected event_item data: {other:?}"),
            },
            other => panic!("unexpected notification: {other:?}"),
        }
    }

    // Exactly one Lost (for the item frame 4's push evicted), then the
    // surviving buffer drains live, FIFO: 3, then 4.
    assert_eq!(
        next_notification(&mut events).await,
        Seen::Lost,
        "frame 4's push should cross the cap and report exactly one Lost"
    );
    assert_eq!(next_notification(&mut events).await, Seen::Frame(3));
    assert_eq!(next_notification(&mut events).await, Seen::Frame(4));

    // rx_buf is now empty: a subsequent push (frame 5) is clean -- no Lost,
    // delivered live immediately, confirming the one drop does not recur.
    let mut frame5 = 0x100_u32.to_be_bytes().to_vec();
    frame5.push(5);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame5, j2534_0404::CAN);
    assert_eq!(
        next_notification(&mut events).await,
        Seen::Frame(5),
        "backlog is empty again; subsequent pushes deliver clean with no further Lost"
    );

    let event_item = client
        .get_event_item(GetEventItemRequest {
            handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("get_event_item should succeed")
        .into_inner()
        .event_item;
    assert!(
        event_item.is_none(),
        "rx_buf should be empty -- everything past the transition was delivered live: \
         {event_item:?}"
    );

    drop(events);
    server.shutdown().await;
}

/// `DiscardNewest`/`PDU_QUE_LIMITED` sibling of
/// `backlog_evicted_then_subscribe_emits_exactly_one_lost_then_drains_fifo`:
/// same stale-backlog-then-subscribe transition, but in the queue mode where
/// Round 1's original self-contradiction bug actually occurred (declaring an
/// item lost while also delivering its own payload). Confirms the discarded
/// item's payload never reaches the live stream at all -- only the
/// surviving pre-existing backlog drains, never the item whose push
/// triggered the drop.
///
/// Sequence (cap 2, `DiscardNewest`):
/// - No subscriber: `ConnectComLogicalLink` enqueues `CllStatus(Online)`
///   (`send_cll_status`, ADR-105 P2 follow-up -- no longer subscription-only)
///   ahead of any frame, filling the first of the cap's 2 slots. P2 backlog
///   follow-up (`docs/implementation-notes.md`): `send_cop_status` now also
///   enqueues into this same FIFO, and `arm_receive_only_monitor` (armed
///   before the frame backlog below, so it can actually observe those
///   frames) queues its own `CopStatus(Executing)` transition right behind
///   it, filling the *second* of the cap's 2 slots (buf `[Online,
///   Executing]`) before any frame is even pushed. Frames 0-3 are therefore
///   each discarded outright -- none of them ever reaches `buf`. No live
///   channel exists to report any discard.
/// - Subscriber attaches. Frame 4 is pushed: buf is still at cap
///   (`[Online, Executing]`), so `DiscardNewest` discards frame 4 itself --
///   it never enters `buf` and is never delivered, live or otherwise.
///   Exactly one `Lost` fires, then the full surviving backlog (`[Online,
///   Executing]`) drains live, FIFO -- frame 4's own payload must never
///   appear on the stream.
/// - Frame 5, pushed once the backlog is empty, delivers clean: no `Lost`,
///   immediate live delivery -- confirming the transition's one drop does
///   not recur on subsequent traffic.
#[tokio::test]
#[serial]
async fn backlog_discarded_then_subscribe_emits_exactly_one_lost_then_drains_fifo() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // A2-5 fix (ADR-126): PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES can now only be
    // used prior to PDUConnect, so the queue is configured on a not-yet-connected
    // CLL before Connect is issued.
    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;
    for &(com_param_id, value) in &[
        (j2534_0404::DATA_RATE, 500_000),
        (CP_REQUEST_ADDR_MODE, 2),
        (CP_CAN_FUNC_REQ_ID, 0x7DF),
    ] {
        set_com_param_unum32(&mut client, cll_handle, com_param_id, value).await;
    }

    let set_queue_props_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_queue_props_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::EventQueueProperty(IoEventQueueProperty {
                    queue_size: 2,
                    queue_mode: PduQueueMode::PduQueLimited as i32,
                })),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES should succeed");

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // No subscriber yet: `CllStatus(Online)` (from the connect above) and
    // the receive-only monitor's own `CopStatus(Executing)` (from arming it,
    // above) already fill both of the cap's 2 slots, so frames 0-3 are each
    // discarded outright by DiscardNewest (buf stays [Online, Executing]).
    // No live channel exists to report any discard.
    for i in 0..4u8 {
        let mut frame = 0x100_u32.to_be_bytes().to_vec();
        frame.push(i);
        server
            .backdoor
            .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Push frame 4 now that a subscriber is live: rx_buf is still at cap
    // ([0, 1]), so DiscardNewest discards frame 4 itself and reports
    // Discarded -- now observable, and frame 4's payload must never be
    // delivered.
    let mut frame4 = 0x100_u32.to_be_bytes().to_vec();
    frame4.push(4);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame4, j2534_0404::CAN);

    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Lost,
        Frame(u8),
        CllOnline,
        CopExecuting,
    }

    async fn next_notification(
        events: &mut tonic::codec::Streaming<vci_service_interface::EventNotification>,
    ) -> Seen {
        let notification =
            tokio::time::timeout(tokio::time::Duration::from_millis(2000), events.message())
                .await
                .expect("should not time out waiting for a notification")
                .expect("stream should not error")
                .expect("stream should not end");
        match notification.event_data {
            Some(event_notification::EventData::Lost(_)) => Seen::Lost,
            Some(event_notification::EventData::Item(item)) => match item.data {
                Some(event_item::Data::ResultData(result)) => Seen::Frame(
                    *result
                        .data_bytes
                        .last()
                        .expect("frame should carry its index byte"),
                ),
                Some(event_item::Data::CllStatus(status))
                    if status == PduComLogicalLinkStatus::PduCllstOnline as i32 =>
                {
                    Seen::CllOnline
                }
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstExecuting as i32 =>
                {
                    Seen::CopExecuting
                }
                other => panic!("unexpected event_item data: {other:?}"),
            },
            other => panic!("unexpected notification: {other:?}"),
        }
    }

    // Exactly one Lost (for the discarded frame 4), then the surviving
    // pre-existing backlog drains live, FIFO: CllStatus(Online), then the
    // receive-only monitor's own CopStatus(Executing). None of frames 0-3
    // ever appears -- each was discarded outright before the subscriber
    // attached, not evicted or buffered. Frame 4's own payload never
    // appears either -- it was discarded, not evicted.
    assert_eq!(
        next_notification(&mut events).await,
        Seen::Lost,
        "frame 4's push should cross the cap and report exactly one Lost"
    );
    assert_eq!(next_notification(&mut events).await, Seen::CllOnline);
    assert_eq!(next_notification(&mut events).await, Seen::CopExecuting);

    // rx_buf is now empty: a subsequent push (frame 5) is clean -- no Lost,
    // delivered live immediately, confirming the one drop does not recur.
    let mut frame5 = 0x100_u32.to_be_bytes().to_vec();
    frame5.push(5);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame5, j2534_0404::CAN);
    assert_eq!(
        next_notification(&mut events).await,
        Seen::Frame(5),
        "backlog is empty again; subsequent pushes deliver clean with no further Lost"
    );

    let event_item = client
        .get_event_item(GetEventItemRequest {
            handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("get_event_item should succeed")
        .into_inner()
        .event_item;
    assert!(
        event_item.is_none(),
        "rx_buf should be empty -- everything past the transition was delivered live: \
         {event_item:?}"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-115 (originally round 4, Codex review PR #122; the underlying
/// mechanism was redesigned in round 6, see that ADR's own "Correction
/// (round 6, ...)" section): `SubscribeEvent` does not require `cll_handle`
/// to already name a live CLL -- a client may subscribe before
/// `CreateComLogicalLink` ever runs for that handle. `rpc_create_com_logical_link`
/// must seed the new queue's `live_sender` from any such
/// already-installed subscription, so the very first frame delivered after
/// creation reaches the pre-existing subscriber live, not just buffered for a
/// later `GetEventItem` poll. `cll_handle` is hardcoded to `1` -- the
/// deterministic first handle `next_logical_link_handle` assigns on a fresh
/// `TestServer` -- since subscribing ahead of creation means there is no
/// handle to read back from a prior RPC response; `create_and_connect_cll`'s
/// own returned handle is asserted against it below as a sanity check.
#[tokio::test]
#[serial]
async fn subscribe_before_create_reconciles_and_delivers_live_after_creation() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    const PRECREATE_CLL_HANDLE: u32 = 1;
    let precreate_handle = ComLogicalLinkHandle {
        module_handle: MOCK_MODULE_HANDLE,
        cll_handle: PRECREATE_CLL_HANDLE,
    };

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(precreate_handle)),
        })
        .await
        .expect("subscribe_event should succeed even though this cll_handle does not exist yet")
        .into_inner();

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    assert_eq!(
        cll_handle.cll_handle, PRECREATE_CLL_HANDLE,
        "sanity check: this test's whole premise is that the pre-subscribed handle is \
         exactly the one CreateComLogicalLink goes on to assign"
    );
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the injected
    // frame deliverable under the unbound-discard model (see
    // `arm_receive_only_monitor`'s own doc comment).
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let mut frame = 0x100_u32.to_be_bytes().to_vec();
    frame.push(0);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);

    // The pre-existing subscriber must see this frame LIVE -- if
    // `rpc_create_com_logical_link` had failed to seed the queue's
    // `live_sender` from the already-installed subscription, this
    // frame would only ever land in the queue (retrievable via
    // `GetEventItem`, never pushed to this stream), and this `.message()`
    // call would time out instead. This subscription was live through
    // `ConnectComLogicalLink` too, so a `CllStatus` notification (Online) may
    // arrive first -- skip anything that isn't the `ResultData` frame this
    // test is actually waiting for.
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(2000);
    let frame_index = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for the live ResultData notification -- the reconciliation \
             at CreateComLogicalLink time must have made this pre-existing subscription live"
        );
        let notification = tokio::time::timeout(remaining, events.message())
            .await
            .expect("should not time out waiting for a notification")
            .expect("stream should not error")
            .expect("stream should not end");
        let is_result_data = matches!(
            &notification.event_data,
            Some(event_notification::EventData::Item(item))
                if matches!(item.data, Some(event_item::Data::ResultData(_)))
        );
        if is_result_data {
            break expect_frame_index_byte(notification);
        }
    };
    assert_eq!(frame_index, 0);

    // Nothing left buffered -- the frame was delivered live, not queued.
    let event_item = client
        .get_event_item(GetEventItemRequest {
            handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("get_event_item should succeed")
        .into_inner()
        .event_item;
    assert!(
        event_item.is_none(),
        "rx_buf should be empty -- the frame was delivered live to the reconciled \
         subscription: {event_item:?}"
    );

    drop(events);
    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 4, fix D): a second CLL
/// must not be allowed to join a physical channel that already has an active
/// client filter (`PDU_IOCTL_START_MSG_FILTER`) installed by another CLL --
/// this is the reciprocal of `ioctl_start_msg_filter`'s own shared-channel
/// rejection (`start_msg_filter_rejects_on_a_shared_physical_channel`): a CLL
/// cannot install a filter once its channel is shared, and now a second CLL
/// cannot join a channel that already carries an active filter either. A
/// `BLOCK_FILTER` in particular would silently drop the joining CLL's
/// traffic channel-wide, so the join itself must be rejected up front.
#[tokio::test]
#[serial]
async fn connect_com_logical_link_rejects_joining_a_channel_with_an_active_client_filter() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a: sole owner of its physical channel, installs a client filter.
    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_a)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_START_MSG_FILTER should succeed while cll_a is still the sole owner");

    // cll_b: same protocol/params as cll_a, which would normally join cll_a's
    // already-open physical channel -- but that channel now has an active
    // client filter, so the join itself must be rejected. Created (not
    // connected) via `create_cll` + a raw `connect_com_logical_link` call,
    // since `create_and_connect_cll` panics on a failing connect.
    let cll_b = create_cll(&mut client, j2534_0404::CAN, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect_err(
            "ConnectComLogicalLink should reject joining a channel with an active client filter",
        );
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(status.message().contains("PDU_ERR_FCT_FAILED"));

    // The rejected join must not have opened a second physical channel.
    assert_eq!(server.backdoor.connect_count(), 1);

    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 4, fix E): `PDU_FLT_PASS`/
/// `PDU_FLT_PASS_UUDT` must be rejected outright by `PDU_IOCTL_START_MSG_FILTER`
/// on any channel -- `connect_new_physical_channel` already installs a
/// wide-open pass-all `PASS_FILTER` on every non-ISO15765 channel so this
/// service's own response matching sees every frame, and `poll_rx_inner`
/// never consults `client_filters` in software, so a narrower client PASS
/// filter would be a silent no-op. `PDU_FLT_BLOCK`/`_BLOCK_UUDT` are
/// unaffected (already exercised by e.g.
/// `start_msg_filter_rejects_duplicate_and_already_installed_filter_numbers`,
/// which installs a BLOCK filter successfully on a non-shared channel).
#[tokio::test]
#[serial]
async fn start_msg_filter_rejects_pass_and_pass_uudt_outright() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let start_filter_count_before = server.backdoor.start_filter_count();

    for (name, filter_type) in [
        ("PDU_FLT_PASS", PduFilter::PduFltPass),
        ("PDU_FLT_PASS_UUDT", PduFilter::PduFltPassUudt),
    ] {
        let status = client
            .io_ctl(IoCtlRequest {
                handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
                io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                    start_filter_id,
                )),
                input_data: Some(DataItem {
                    data: Some(data_item::Data::FilterData(IoFilterList {
                        filters: vec![IoFilter {
                            filter_type: filter_type as i32,
                            filter_number: 1,
                            filter_mask_message: vec![0, 0, 0, 0],
                            filter_pattern_message: vec![0, 0, 0, 0],
                        }],
                    })),
                }),
                has_output: false,
            })
            .await
            .expect_err(&format!("{name} should be rejected outright"));
        assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{name}");
        assert!(
            status.message().contains("PDU_ERR_FCT_FAILED"),
            "{name}: unexpected message {}",
            status.message()
        );
    }

    assert_eq!(
        server.backdoor.start_filter_count(),
        start_filter_count_before,
        "no PASS/PASS_UUDT filter should ever reach the hardware"
    );

    server.shutdown().await;
}

/// Backlog closure (the Prioritized Backlog, A2-7/
/// ADR-129 follow-up entry): `ioctl_start_msg_filter`'s empty-
/// `PDU_IO_FILTER_LIST` rejection (`rpc_misc.rs`) has no coverage at all --
/// this is the very first check in the function, and runs pre-connect just
/// as much as connected (ISO 22900-2 §9.4.11.2 d) allows configuring this
/// IOCTL before `PDUConnect`, mirroring
/// `start_msg_filter_before_connect_is_stored_pending_and_installed_on_connect`).
/// Covers both shapes the function's `match` treats identically: an
/// explicitly empty `FilterData` list, and a missing `input_data` entirely.
/// Unlike every other rejection in this file, this one is a bare
/// `Status::invalid_argument` (not `state_guard_status`), so it carries no
/// `ErrorDetail`/`PduError` at all -- asserted explicitly below rather than
/// skipped, so a future switch to `state_guard_status` doesn't go unnoticed.
#[tokio::test]
#[serial]
async fn start_msg_filter_rejects_an_empty_filter_list() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let start_filter_count_before = server.backdoor.start_filter_count();

    // Explicitly empty PDU_IO_FILTER_LIST.
    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![],
                })),
            }),
            has_output: false,
        })
        .await
        .expect_err("an empty PDU_IO_FILTER_LIST should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        error_detail_from_status(&status).is_none(),
        "this rejection is a bare Status::invalid_argument, not state_guard_status, so it \
         carries no ErrorDetail"
    );

    // Missing input_data entirely -- the same `match` arm handles both.
    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: None,
            has_output: false,
        })
        .await
        .expect_err("a missing input_data should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        error_detail_from_status(&status).is_none(),
        "this rejection is a bare Status::invalid_argument, not state_guard_status, so it \
         carries no ErrorDetail"
    );

    assert_eq!(
        server.backdoor.start_filter_count(),
        start_filter_count_before,
        "no filter should reach the hardware when the request has an empty/missing filter list"
    );

    server.shutdown().await;
}

/// Backlog closure (the Prioritized Backlog, A2-7/
/// ADR-129 follow-up entry): `ioctl_start_msg_filter`'s ISO15765
/// `base_hw_protocol_id` rejection (ADR-038) has no coverage at all.
/// PASS_FILTER/BLOCK_FILTER-based filter types are not supported on an
/// ISO15765 link -- `FLOW_CONTROL_FILTER` is the only filter type this
/// service installs there -- so an ordinary `PDU_FLT_BLOCK` request must be
/// rejected with `PDU_ERR_VALUE_NOT_SUPPORTED`, distinct from
/// `start_msg_filter_rejects_pass_and_pass_uudt_outright` above (a
/// different check, on a different filter type, applying to any protocol).
#[tokio::test]
#[serial]
async fn start_msg_filter_rejects_pass_block_filters_on_an_iso15765_link() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let start_filter_count_before = server.backdoor.start_filter_count();

    let one_filter = |filter_number: u32| IoFilter {
        filter_type: PduFilter::PduFltBlock as i32,
        filter_number,
        filter_mask_message: vec![0, 0, 0, 0],
        filter_pattern_message: vec![0, 0, 0, 0],
    };

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(1)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect_err(
            "a PASS_FILTER/BLOCK_FILTER-based filter should be rejected on an ISO15765 link",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrValueNotSupported as i32
    );
    assert_eq!(
        server.backdoor.start_filter_count(),
        start_filter_count_before,
        "no filter should reach the hardware when the link is ISO15765"
    );

    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 6): `PDU_IOCTL_RESET` must
/// stop every filter tracked in each CLL's `client_filters` map, and must
/// remove each `FilterNumber` from the map once its hardware filter actually
/// stopped -- mirrors the same fix already made and tested for
/// `PDU_IOCTL_CLEAR_MSG_FILTER`
/// (`clear_msg_filter_stops_all_client_filters_and_empties_the_map`). This
/// only covers the success path: the current mock harness has no mechanism to
/// make a specific `PassThruStopMsgFilter` call fail (only
/// `set_fast_init_error`, which is wired to `IOCTL_FAST_INIT`), so the
/// keep-on-failure branch itself is not exercised here.
#[tokio::test]
#[serial]
async fn reset_stops_all_client_filters_and_empties_the_map() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    let reset_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESET").await;
    let one_filter = |filter_number: u32| IoFilter {
        filter_type: PduFilter::PduFltBlock as i32,
        filter_number,
        filter_mask_message: vec![0, 0, 0, 0],
        filter_pattern_message: vec![0, 0, 0, 0],
    };

    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(1)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("installing FilterNumber 1 should succeed");

    let stop_filter_count_before = server.backdoor.stop_filter_count();

    io_ctl_module(&mut client, reset_id)
        .await
        .expect("PDU_IOCTL_RESET should succeed");

    assert_eq!(
        server.backdoor.stop_filter_count() - stop_filter_count_before,
        2,
        "PDU_IOCTL_RESET should stop the installed filter -- installed as 2 hardware filters \
         (TxFlags 0 and TX_EXTENDED_ID) because raw CAN always connects CAN_ID_BOTH (ADR-065)"
    );

    // Reusing FilterNumber 1 must now succeed: client_filters must be empty,
    // not still tracking it as already-installed.
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![one_filter(1)],
                })),
            }),
            has_output: false,
        })
        .await
        .expect(
            "reusing FilterNumber 1 after PDU_IOCTL_RESET should succeed, proving \
             client_filters ended up empty",
        );

    server.shutdown().await;
}

/// ADR-161 Phase 1 (shared-physical-channel occupancy-epoch check): two
/// ComLogicalLinks sharing one physical channel must both survive
/// `PDU_IOCTL_RESET`'s hardware teardown, and the channel-wide RX/TX buffer
/// clears must run exactly once for the channel -- not once per CLL sharing
/// it, which the pre-fix per-target loop did. Both CLLs join during setup
/// (cll_b's join re-stamps the channel's `occupancy_epoch`), so RESET's own
/// snapshot observes the settled epoch and the clears proceed normally; this
/// is the happy-path, no-concurrent-join case. Neither CLL can hold a client
/// message filter here: installing one on an already-shared channel is
/// rejected (`start_msg_filter_rejects_on_a_shared_physical_channel`), and
/// joining a channel that already carries one is rejected too
/// (`connect_com_logical_link_rejects_joining_a_channel_with_an_active_client_filter`)
/// -- so this scenario cannot exercise the per-target filter-stop loop
/// (already covered, on a sole-owner channel, by
/// `reset_stops_all_client_filters_and_empties_the_map`); it covers the
/// buffer-clear grouping instead. The actual race this ADR closes (a
/// sibling's join completing between the snapshot and Phase 1 reaching this
/// channel) has no deterministic reproduction in this harness (ADR-161's own
/// accepted-residual precedent).
#[tokio::test]
#[serial]
async fn reset_on_a_shared_physical_channel_clears_buffers_once_and_keeps_both_clls_connected() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

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

    let reset_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESET").await;

    let clear_rx_before = server.backdoor.clear_rx_buffer_count();
    let clear_tx_before = server.backdoor.clear_tx_buffer_count();
    let disconnect_count_before = server.backdoor.disconnect_count();

    io_ctl_module(&mut client, reset_id)
        .await
        .expect("PDU_IOCTL_RESET should succeed on a shared physical channel");

    assert_eq!(
        server.backdoor.clear_rx_buffer_count() - clear_rx_before,
        1,
        "the channel-wide RX buffer clear must run exactly once for the shared channel, not \
         once per ComLogicalLink sharing it (ADR-161 Phase 1)"
    );
    assert_eq!(
        server.backdoor.clear_tx_buffer_count() - clear_tx_before,
        1,
        "the channel-wide TX buffer clear must run exactly once for the shared channel, not \
         once per ComLogicalLink sharing it (ADR-161 Phase 1)"
    );
    assert_eq!(
        server.backdoor.disconnect_count(),
        disconnect_count_before,
        "PDU_IOCTL_RESET must not disconnect either ComLogicalLink sharing the channel"
    );

    // Both CLLs must still be live and independently usable after RESET.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("cll_a should still be connected and disconnectable after PDU_IOCTL_RESET");
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("cll_b should still be connected and disconnectable after PDU_IOCTL_RESET");

    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 7, fix I):
/// `ensure_uudt_companion_channel` (the `can_channel_mode = "dual-channel"`
/// path that opens/joins the raw-CAN companion channel used for UUDT
/// reception on an ISO15765 CLL) must not silently join a `(CAN, baud)`
/// physical channel that already carries an active client
/// `PDU_IOCTL_START_MSG_FILTER` filter installed by another CLL -- the same
/// reciprocal guard already covered for the primary-channel join path by
/// `connect_com_logical_link_rejects_joining_a_channel_with_an_active_client_filter`.
///
/// Both call sites of `ensure_uudt_companion_channel` in dual-channel mode
/// (`rpc_connect_com_logical_link` and the `SetUniqueRespIdTable` promotion
/// flow in `rpc_link.rs`) only `warn!` on its error rather than propagating
/// it to the RPC caller -- there is no ADR-041 point-to-point-filter
/// fallback for explicit `"dual-channel"` mode (unlike `"auto"` mode's
/// incapable-probe fallback), so the rejection's only observable effect is
/// that the companion channel is never opened: no third `PassThruConnect`,
/// and a UUDT frame injected on the raw-CAN CLL's channel never reaches the
/// ISO15765 CLL.
#[tokio::test]
#[serial]
async fn ensure_uudt_companion_channel_rejects_joining_a_channel_with_an_active_client_filter() {
    let server = TestServer::start_with_can_mode(Some("dual-channel")).await;
    let mut client = server.client().await;

    // cll_a: raw-CAN CLL, sole owner of its physical (CAN, 500_000) channel,
    // installs a client filter.
    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let start_filter_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_MSG_FILTER").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_a)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_START_MSG_FILTER should succeed while cll_a is still the sole owner");

    assert_eq!(server.backdoor.connect_count(), 1);

    // cll_b: ISO15765 CLL at the same baud rate -- its own primary channel is
    // a separate PassThruConnect from cll_a's raw-CAN channel, so this
    // connect itself succeeds even though it shares no channel with cll_a
    // yet.
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 2);

    // Configuring a UUDT response ID on cll_b triggers
    // ensure_uudt_companion_channel, which would normally join cll_a's
    // already-open (CAN, 500_000) channel for UUDT reception -- but that
    // channel now carries an active client filter, so the join must be
    // rejected and no companion channel opened.
    set_unique_resp_table_and_promote(
        &mut client,
        cll_b,
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

    // No third PassThruConnect: the companion-channel join was rejected
    // outright rather than silently opening a brand-new channel instead.
    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the rejected companion-channel join must not open a third physical channel"
    );

    // A UUDT frame injected on cll_a's channel never reaches cll_b: with no
    // companion channel, cll_b has no path to receive it.
    let uudt_payload = vec![0x62, 0xF1, 0x90, 0xAA];
    let mut uudt = 0x5E8_u32.to_be_bytes().to_vec();
    uudt.extend_from_slice(&uudt_payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &uudt, j2534_0404::CAN);
    assert_no_result_data(
        &mut client,
        cll_b,
        "UUDT frame must not reach cll_b: its companion-channel join was rejected",
    )
    .await;

    server.shutdown().await;
}

/// Verified Codex-review regression (PR #80, round 7, fix J): `rpc_get_event_item`'s
/// `CllHandle` branch must enforce `PDU_IOCTL_SET_BUFFER_SIZE`'s
/// `result_buffer_limit` by truncating `ResultData.data_bytes` to at most
/// that many bytes -- distinct from `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`,
/// which caps the number of buffered *items*, not each item's payload size.
#[tokio::test]
#[serial]
async fn get_event_item_truncates_data_to_the_buffer_size_limit() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    // ADR-100 Decision §5 (S8): a receive-only monitor keeps the injected
    // frame below delivered under the new unbound-discard model (see
    // `arm_receive_only_monitor`'s own doc comment).
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let set_buffer_size_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_BUFFER_SIZE").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_buffer_size_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::Unum32Value(2)),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_SET_BUFFER_SIZE should succeed");

    // No UniqueRespIdTable is configured, so routing delivers the injected
    // frame unconditionally (see rx_header_split.rs).
    let mut frame = 0x100_u32.to_be_bytes().to_vec();
    frame.extend_from_slice(&[1, 2, 3, 4, 5]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);

    // Give the poll task time to dequeue the frame into rx_buf before
    // draining it via GetEventItem.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // ADR-105 P2 follow-up: `create_and_connect_cll` above also enqueued a
    // `CllStatus(Online)` entry (`send_cll_status`, no longer
    // subscription-only) ahead of the injected frame -- drain and confirm
    // it first, so it is not mistaken for the frame under test below.
    let leading = client
        .get_event_item(GetEventItemRequest {
            handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("get_event_item should succeed")
        .into_inner()
        .event_item
        .expect("the CllStatus(Online) entry enqueued at connect should be buffered");
    assert!(
        matches!(
            leading.data,
            Some(event_item::Data::CllStatus(status))
                if status == PduComLogicalLinkStatus::PduCllstOnline as i32
        ),
        "the FIFO head should be the CllStatus(Online) transition enqueued at connect: {leading:?}"
    );

    // P2 backlog follow-up (`docs/implementation-notes.md`): `send_cop_status`
    // now also enqueues into this same `rx_buf` FIFO (no longer
    // subscription-only), so the receive-only monitor's own
    // `CopStatus(Executing)` transition is queued right behind
    // `CllStatus(Online)` -- drain and confirm it too, so it is not mistaken
    // for the frame under test below.
    let leading_cop_status = client
        .get_event_item(GetEventItemRequest {
            handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("get_event_item should succeed")
        .into_inner()
        .event_item
        .expect(
            "the CopStatus(Executing) entry enqueued when arming the monitor should be buffered",
        );
    assert!(
        matches!(
            leading_cop_status.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstExecuting as i32
        ),
        "the second FIFO entry should be the receive-only monitor's own CopStatus(Executing) \
         transition: {leading_cop_status:?}"
    );

    let result = client
        .get_event_item(GetEventItemRequest {
            handle: Some(get_event_item_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("get_event_item should succeed")
        .into_inner()
        .event_item
        .map(|item| match item.data {
            Some(event_item::Data::ResultData(result)) => result,
            other => panic!("unexpected event_item data: {other:?}"),
        })
        .expect("a buffered frame should be available");

    assert_eq!(
        result.data_bytes.len(),
        2,
        "GetEventItem must truncate data_bytes to the PDU_IOCTL_SET_BUFFER_SIZE limit"
    );
    assert_eq!(result.data_bytes, vec![1, 2]);

    server.shutdown().await;
}

// ── ADR-123 (A2-15) coverage gaps (edge-case-hunter verification pass) ─────

/// Coverage gap: `PDU_IOCTL_RESET` must not let a client bypass a sibling's
/// held `LOCK_PHYSICAL_TX_QUEUE`. `ioctl_reset` operates module-wide (every
/// live `LogicalLinkState`, addressed via a `module_handle`, per its own doc
/// comment), and `events::cancel_held_tx_items`'s `reset_suspended` clears
/// only `tx_suspended_by_ioctl` -- `tx_suspended_by_lock` is untouched. It
/// still unconditionally drains/cancels `tx_held` regardless of which flag
/// caused the item to be held, so a COP held only because of cll_a's lock is
/// CANCELLED (`PduCopstCancelled`), never dispatched -- matching
/// `reset_drains_held_tx_queue_and_cancels_it`'s existing outcome for the
/// IOCTL-suspend source, verified here for the lock-suspend source instead.
#[tokio::test]
#[serial]
async fn reset_does_not_dispatch_a_lock_held_sibling_cop_but_cancels_it() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = TestServer::start().await;
    let mut client = server.client().await;

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

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("lock_resource should succeed");

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_send_recv(&mut client, cll_b, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "cll_b's CoptSendrecv must be queued (tx_suspended_by_lock), not dispatched, while \
         cll_a holds the TX queue lock"
    );

    let reset_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESET").await;
    io_ctl_module(&mut client, reset_id)
        .await
        .expect("PDU_IOCTL_RESET should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "cll_b's lock-held COP must be cancelled by PDU_IOCTL_RESET, not silently dropped"
    );
    drop(events);

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "PDU_IOCTL_RESET must never dispatch a COP held only because of a sibling's \
         LOCK_PHYSICAL_TX_QUEUE -- cancellation, not a bypass, is the only outcome"
    );

    // Confirm the cancellation was real, not merely a delay: even after
    // cll_a later releases its lock, nothing dispatches -- the item is gone.
    client
        .unlock_resource(UnlockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("unlock_resource should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the cancelled COP must never be dispatched, even after cll_a later releases its lock"
    );

    server.shutdown().await;
}

/// Coverage gap (ADR-123 §3, two independent suspension sources): a CLL
/// suspended by BOTH its own `PDU_IOCTL_SUSPEND_TX_QUEUE`
/// (`tx_suspended_by_ioctl`) AND a sibling's held `LOCK_PHYSICAL_TX_QUEUE`
/// (`tx_suspended_by_lock`) stays held after only one source clears --
/// `LogicalLinkState::tx_suspended()` is the OR of both flags. Clearing
/// `tx_suspended_by_ioctl` alone (via `PDU_IOCTL_RESUME_TX_QUEUE`) must not
/// dispatch the queued COP; only releasing the sibling's lock (which clears
/// `tx_suspended_by_lock` too, via the `recompute_lock_tx_suspensions`
/// sweep) does.
#[tokio::test]
#[serial]
async fn queued_cop_stays_held_until_both_ioctl_and_lock_suspension_sources_clear() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = TestServer::start().await;
    let mut client = server.client().await;

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

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;

    // cll_b's own client-driven suspension: tx_suspended_by_ioctl = true.
    io_ctl_cll(&mut client, cll_b, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    // cll_a's TX-queue lock recomputes cll_b's tx_suspended_by_lock = true
    // too, since it shares the physical resource and does not hold the lock.
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("lock_resource should succeed");

    start_send_recv(&mut client, cll_b, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "cll_b's COP should be held: both tx_suspended_by_ioctl and tx_suspended_by_lock are true"
    );

    // Clearing only tx_suspended_by_ioctl must not dispatch the COP --
    // tx_suspended_by_lock (owned exclusively by cll_a's held lock) is still
    // true.
    io_ctl_cll(&mut client, cll_b, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "cll_b's COP must stay held after its own PDU_IOCTL_RESUME_TX_QUEUE -- cll_a's held \
         LOCK_PHYSICAL_TX_QUEUE still suspends it"
    );

    // Releasing cll_a's lock clears tx_suspended_by_lock too -- now
    // tx_suspended() is false and the COP dispatches.
    client
        .unlock_resource(UnlockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("unlock_resource should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.shutdown().await;
}

/// A2-8 completeness fix (Codex review, PR #143): the module-scoped, but
/// unsupported-on-this-adapter, `PDU_IOCTL_GENERIC`/`GET_CABLE_ID`/
/// `READ_IGNITION_SENSE_STATE` commands used to validate only `module_handle`
/// range and then unconditionally return `PDU_ERR_ID_NOT_SUPPORTED`/
/// `PDU_ERR_CABLE_UNKNOWN`, never checking whether the module was connected
/// at all -- inconsistent with the other module-scoped IOCTLs this A2-8 fix
/// covers. ISO 22900-2 §9.4.29.2 NOTE 1 applies to every `PDUIoCtl` call
/// regardless of which command ID it names, so these three must also reject
/// with `PDU_ERR_MODULE_NOT_CONNECTED` before a device is ever open.
#[tokio::test]
#[serial]
async fn module_scoped_unsupported_ioctls_reject_when_module_not_connected() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let generic_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GENERIC").await;
    let cable_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_CABLE_ID").await;
    let ignition_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_IGNITION_SENSE_STATE").await;

    for (id, label) in [
        (generic_id, "PDU_IOCTL_GENERIC"),
        (cable_id, "PDU_IOCTL_GET_CABLE_ID"),
        (ignition_id, "PDU_IOCTL_READ_IGNITION_SENSE_STATE"),
    ] {
        let status = io_ctl_module(&mut client, id)
            .await
            .expect_err(&format!("{label} should be rejected before ModuleConnect"));
        assert_eq!(
            status.code(),
            tonic::Code::FailedPrecondition,
            "{label} should reject with FailedPrecondition when the module isn't connected, not \
             its usual Unimplemented -- connection state is checked first"
        );
        let detail =
            error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
        assert_eq!(
            detail.pdu_error,
            PduError::PduErrModuleNotConnected as i32,
            "{label} should carry PDU_ERR_MODULE_NOT_CONNECTED"
        );
    }

    server.shutdown().await;
}

/// A2-8 regression: the module-scoped `PDU_IOCTL_RESET`/`READ_VBATT`/
/// `SET_PROG_VOLTAGE`/`READ_PROG_VOLTAGE` commands used to either lazily open
/// the device (`READ_VBATT`/`SET_PROG_VOLTAGE`/`READ_PROG_VOLTAGE`, via
/// `ensure_open_device_for`) or silently proceed as a no-op (`RESET`, via
/// `lock_device_for`) when the target module had never been connected via
/// `ModuleConnect` -- instead of rejecting with `PDU_ERR_MODULE_NOT_CONNECTED`
/// as ISO 22900-2 §9.4.29.2 NOTE 1 / Table 12 require for every `PDUIoCtl`
/// call other than the small NOTE-1 allow-list (none of which are IOCTLs).
#[tokio::test]
#[serial]
async fn module_scoped_ioctls_reject_when_module_not_connected() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let reset_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESET").await;
    let read_vbatt_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_VBATT").await;
    let set_prog_voltage_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_PROG_VOLTAGE").await;
    let read_prog_voltage_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_PROG_VOLTAGE").await;

    let assert_module_not_connected = |status: tonic::Status, label: &str| {
        assert_eq!(
            status.code(),
            tonic::Code::FailedPrecondition,
            "{label} should reject with FailedPrecondition when the module isn't connected"
        );
        let detail =
            error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
        assert_eq!(
            detail.pdu_error,
            PduError::PduErrModuleNotConnected as i32,
            "{label} should carry PDU_ERR_MODULE_NOT_CONNECTED"
        );
    };

    let status = io_ctl_module(&mut client, reset_id)
        .await
        .expect_err("PDU_IOCTL_RESET should be rejected before ModuleConnect");
    assert_module_not_connected(status, "PDU_IOCTL_RESET");

    let status = io_ctl_module(&mut client, read_vbatt_id)
        .await
        .expect_err("PDU_IOCTL_READ_VBATT should be rejected before ModuleConnect");
    assert_module_not_connected(status, "PDU_IOCTL_READ_VBATT");

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_prog_voltage_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::ProgVoltage(
                    vci_service_interface::IoProgVoltage {
                        prog_voltage_mv: 5000,
                        pin_on_dlc: 6,
                    },
                )),
            }),
            has_output: false,
        })
        .await
        .expect_err("PDU_IOCTL_SET_PROG_VOLTAGE should be rejected before ModuleConnect");
    assert_module_not_connected(status, "PDU_IOCTL_SET_PROG_VOLTAGE");

    let status = io_ctl_module(&mut client, read_prog_voltage_id)
        .await
        .expect_err("PDU_IOCTL_READ_PROG_VOLTAGE should be rejected before ModuleConnect");
    assert_module_not_connected(status, "PDU_IOCTL_READ_PROG_VOLTAGE");

    server.shutdown().await;
}

/// A2-8 completeness fix (Codex review, PR #143): `PDU_IOCTL_SET_PROG_VOLTAGE`
/// used to validate its `input_data` payload before checking connection
/// state, so a disconnected module with missing/malformed input got
/// `InvalidArgument` instead of `PDU_ERR_MODULE_NOT_CONNECTED` -- inconsistent
/// with the other 6 module-scoped IOCTLs, where the connection check always
/// runs first. `ioctl_set_prog_voltage` now checks connection before parsing
/// the payload.
#[tokio::test]
#[serial]
async fn set_prog_voltage_rejects_module_not_connected_even_with_missing_input_data() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let set_prog_voltage_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_PROG_VOLTAGE").await;

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_prog_voltage_id,
            )),
            input_data: None,
            has_output: false,
        })
        .await
        .expect_err(
            "PDU_IOCTL_SET_PROG_VOLTAGE with no input_data should still be rejected before \
             ModuleConnect",
        );
    assert_eq!(
        status.code(),
        tonic::Code::FailedPrecondition,
        "connection state must be checked before input_data validation, so a disconnected \
         module reports FailedPrecondition/MODULE_NOT_CONNECTED, not InvalidArgument"
    );
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrModuleNotConnected as i32,
        "PDU_IOCTL_SET_PROG_VOLTAGE should carry PDU_ERR_MODULE_NOT_CONNECTED even with no \
         input_data"
    );

    server.shutdown().await;
}

/// A2-8 regression, positive case: once `ModuleConnect` has actually opened
/// the device, `READ_VBATT`/`SET_PROG_VOLTAGE`/`READ_PROG_VOLTAGE`/`RESET`
/// keep working exactly as before -- the fix only rejects the
/// never-connected case, it does not newly require some OTHER connection
/// step beyond `ModuleConnect`.
#[tokio::test]
#[serial]
async fn module_scoped_ioctls_succeed_once_module_connected() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let reset_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESET").await;
    let read_vbatt_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_VBATT").await;
    let set_prog_voltage_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_PROG_VOLTAGE").await;
    let read_prog_voltage_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_PROG_VOLTAGE").await;

    io_ctl_module(&mut client, read_vbatt_id)
        .await
        .expect("PDU_IOCTL_READ_VBATT should succeed once the module is connected");

    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_prog_voltage_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::ProgVoltage(
                    vci_service_interface::IoProgVoltage {
                        prog_voltage_mv: 5000,
                        pin_on_dlc: 6,
                    },
                )),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_SET_PROG_VOLTAGE should succeed once the module is connected");

    io_ctl_module(&mut client, read_prog_voltage_id)
        .await
        .expect("PDU_IOCTL_READ_PROG_VOLTAGE should succeed once the module is connected");

    io_ctl_module(&mut client, reset_id)
        .await
        .expect("PDU_IOCTL_RESET should succeed once the module is connected");

    server.shutdown().await;
}

/// A2-21 (ISO 22900-2:2009 Table 49): a `PassThruSetProgrammingVoltage`
/// failure with any native status OTHER than `ERR_PIN_INVALID` still maps to
/// `PDU_ERR_VOLTAGE_NOT_SUPPORTED`, unchanged from before A2-21 -- only the
/// pin-invalid case (covered separately below) gets the new
/// `PDU_ERR_MUX_RSC_NOT_SUPPORTED` mapping.
#[tokio::test]
#[serial]
async fn set_prog_voltage_generic_native_failure_still_maps_to_voltage_not_supported() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let set_prog_voltage_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_PROG_VOLTAGE").await;

    // ERR_FAILED (0x01) -- not ERR_PIN_INVALID -- must still map to
    // PDU_ERR_VOLTAGE_NOT_SUPPORTED.
    server.backdoor.set_prog_voltage_error(Some(1));

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_prog_voltage_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::ProgVoltage(
                    vci_service_interface::IoProgVoltage {
                        prog_voltage_mv: 5000,
                        pin_on_dlc: 6,
                    },
                )),
            }),
            has_output: false,
        })
        .await
        .expect_err(
            "PDU_IOCTL_SET_PROG_VOLTAGE should fail when the native call is forced to fail",
        );
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrVoltageNotSupported as i32,
        "a generic native failure must still map to PDU_ERR_VOLTAGE_NOT_SUPPORTED"
    );

    server.backdoor.set_prog_voltage_error(None);
    server.shutdown().await;
}

/// A2-21 (ISO 22900-2:2009 Table 49): a `PassThruSetProgrammingVoltage`
/// failure whose native status is specifically `ERR_PIN_INVALID` maps to
/// `PDU_ERR_MUX_RSC_NOT_SUPPORTED` (invalid pin/resource) instead of the
/// generic `PDU_ERR_VOLTAGE_NOT_SUPPORTED` (unsupported voltage) -- these are
/// distinct codes per Table 49, and only the native layer can distinguish
/// them.
#[tokio::test]
#[serial]
async fn set_prog_voltage_pin_invalid_native_failure_maps_to_mux_rsc_not_supported() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let set_prog_voltage_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_PROG_VOLTAGE").await;

    server
        .backdoor
        .set_prog_voltage_error(Some(j2534_0404::ERR_PIN_INVALID as _));

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_prog_voltage_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::ProgVoltage(
                    vci_service_interface::IoProgVoltage {
                        prog_voltage_mv: 5000,
                        pin_on_dlc: 6,
                    },
                )),
            }),
            has_output: false,
        })
        .await
        .expect_err("PDU_IOCTL_SET_PROG_VOLTAGE should fail when ERR_PIN_INVALID is forced");
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrMuxRscNotSupported as i32,
        "ERR_PIN_INVALID must map to PDU_ERR_MUX_RSC_NOT_SUPPORTED, not \
         PDU_ERR_VOLTAGE_NOT_SUPPORTED"
    );

    server.backdoor.set_prog_voltage_error(None);
    server.shutdown().await;
}

/// SAE J2534-2 clause 15 (Phase 13): a `PassThruSetProgrammingVoltage`
/// failure whose native status is `ERR_PIN_IN_USE` -- setting voltage on pin
/// 9 while it is grounded, or vice versa -- maps to `PDU_ERR_RESOURCE_BUSY`,
/// matching the `ERR_CHANNEL_IN_USE` precedent, not the generic
/// `PDU_ERR_VOLTAGE_NOT_SUPPORTED`.
#[tokio::test]
#[serial]
async fn set_prog_voltage_pin_in_use_native_failure_maps_to_resource_busy() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let set_prog_voltage_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_PROG_VOLTAGE").await;

    server
        .backdoor
        .set_prog_voltage_error(Some(j2534_0404::ERR_PIN_IN_USE as _));

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_prog_voltage_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::ProgVoltage(
                    vci_service_interface::IoProgVoltage {
                        prog_voltage_mv: 5000,
                        pin_on_dlc: 9,
                    },
                )),
            }),
            has_output: false,
        })
        .await
        .expect_err("PDU_IOCTL_SET_PROG_VOLTAGE should fail when ERR_PIN_IN_USE is forced");
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrResourceBusy as i32,
        "ERR_PIN_IN_USE must map to PDU_ERR_RESOURCE_BUSY, not PDU_ERR_VOLTAGE_NOT_SUPPORTED"
    );

    server.backdoor.set_prog_voltage_error(None);
    server.shutdown().await;
}

/// SAE J2534-2 clause 15 (Phase 13): a `PassThruSetProgrammingVoltage`
/// failure whose native status is `ERR_VOLTAGE_IN_USE` -- grounding pin 9
/// while pin 15 is grounded, or vice versa -- maps to `PDU_ERR_RESOURCE_BUSY`,
/// matching the `ERR_CHANNEL_IN_USE` precedent, not the generic
/// `PDU_ERR_VOLTAGE_NOT_SUPPORTED`.
#[tokio::test]
#[serial]
async fn set_prog_voltage_voltage_in_use_native_failure_maps_to_resource_busy() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let set_prog_voltage_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_PROG_VOLTAGE").await;

    server
        .backdoor
        .set_prog_voltage_error(Some(j2534_0404::ERR_VOLTAGE_IN_USE as _));

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                set_prog_voltage_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::ProgVoltage(
                    vci_service_interface::IoProgVoltage {
                        prog_voltage_mv: 0,
                        pin_on_dlc: 9,
                    },
                )),
            }),
            has_output: false,
        })
        .await
        .expect_err("PDU_IOCTL_SET_PROG_VOLTAGE should fail when ERR_VOLTAGE_IN_USE is forced");
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrResourceBusy as i32,
        "ERR_VOLTAGE_IN_USE must map to PDU_ERR_RESOURCE_BUSY, not PDU_ERR_VOLTAGE_NOT_SUPPORTED"
    );

    server.backdoor.set_prog_voltage_error(None);
    server.shutdown().await;
}

/// A2-8 accepted-residual regression (Codex review, PR #143; ADR-107 Accepted
/// Residual #5): `require_connected_device_for` gates on "a device is open
/// under this handle," not on whether `ModuleConnect` was the literal RPC
/// that opened it. `CreateComLogicalLink`'s own pre-existing lazy-open
/// (ADR-107 Decision (d)) also satisfies it -- deliberately, not a gap --
/// since ISO 22900-2 §9.4.29.2 NOTE 1 keys `PDU_ERR_MODULE_NOT_CONNECTED` to
/// the module's status, not to call history, and this service's
/// READY-equivalent state is exactly "device open." This pins that intended
/// behavior so it reads as an asserted contract, not an accident.
#[tokio::test]
#[serial]
async fn module_scoped_ioctls_succeed_after_lazy_open_via_create_cll() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // No `ModuleConnect` call -- `create_cll` lazily opens the device via
    // `CreateComLogicalLink`'s own `ensure_open_device_for` (ADR-107).
    let _cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;

    let reset_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESET").await;
    let read_vbatt_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_VBATT").await;

    io_ctl_module(&mut client, read_vbatt_id).await.expect(
        "PDU_IOCTL_READ_VBATT should succeed after a lazy open via CreateComLogicalLink, with \
         no prior ModuleConnect call",
    );
    io_ctl_module(&mut client, reset_id).await.expect(
        "PDU_IOCTL_RESET should succeed after a lazy open via CreateComLogicalLink, with no \
         prior ModuleConnect call",
    );

    server.shutdown().await;
}

// ── SAE J2534-2 clause 23 J1962 Pin Voltage Read (Phase 13) ────────────────

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// required for `PDU_IOCTL_READ_J1962PIN_VOLTAGE` (Codex review, PR #48: this
/// gate was originally missing). Mirrors `repeat_message.rs`'s own
/// `start_j2534_2_server` helper (not shared via `harness.rs`, matching this
/// codebase's existing per-file-helper convention) -- `start_with_modules`'s
/// single entry gets module_handle 1, matching `MOCK_MODULE_HANDLE`.
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

fn unum32_input(value: u32) -> DataItem {
    DataItem {
        data: Some(data_item::Data::Unum32Value(value)),
    }
}

async fn read_j1962_pin_voltage(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cmd_id: u32,
    pin_number: u32,
) -> Result<u32, tonic::Status> {
    let output = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data: Some(unum32_input(pin_number)),
            has_output: true,
        })
        .await?
        .into_inner()
        .output_data;
    match output.and_then(|d| d.data) {
        Some(data_item::Data::Unum32Value(mv)) => Ok(mv),
        other => panic!(
            "PDU_IOCTL_READ_J1962PIN_VOLTAGE should return a Unum32Value voltage, got {other:?}"
        ),
    }
}

/// (a) A successful read on a supported pin (pin 1) returns the mock's fixed
/// `MOCK_J1962_PIN_VOLTAGE_MV` reading.
#[tokio::test]
#[serial]
async fn read_j1962_pin_voltage_returns_mock_value_for_supported_pin() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let read_pin_voltage_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_J1962PIN_VOLTAGE").await;

    let mv = read_j1962_pin_voltage(&mut client, read_pin_voltage_id, 1)
        .await
        .expect("PDU_IOCTL_READ_J1962PIN_VOLTAGE should succeed for pin 1");
    assert_eq!(
        mv, 5000,
        "pin 1 should report the mock's fixed J1962 pin voltage reading"
    );

    server.shutdown().await;
}

/// (b) Pin 16 must report the same voltage `PDU_IOCTL_READ_VBATT` would, per
/// SAE J2534-2 clause 23.
#[tokio::test]
#[serial]
async fn read_j1962_pin_voltage_pin_16_matches_read_vbatt() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let read_vbatt_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_VBATT").await;
    let read_pin_voltage_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_J1962PIN_VOLTAGE").await;

    let vbatt_output = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                read_vbatt_id,
            )),
            input_data: None,
            has_output: true,
        })
        .await
        .expect("PDU_IOCTL_READ_VBATT should succeed")
        .into_inner()
        .output_data;
    let vbatt_mv = match vbatt_output.and_then(|d| d.data) {
        Some(data_item::Data::Unum32Value(mv)) => mv,
        other => panic!("PDU_IOCTL_READ_VBATT should return a Unum32Value voltage, got {other:?}"),
    };

    let pin16_mv = read_j1962_pin_voltage(&mut client, read_pin_voltage_id, 16)
        .await
        .expect("PDU_IOCTL_READ_J1962PIN_VOLTAGE should succeed for pin 16");
    assert_eq!(
        pin16_mv, vbatt_mv,
        "pin 16 must report the same voltage PDU_IOCTL_READ_VBATT does"
    );

    server.shutdown().await;
}

/// (c) Pin 4 and pin 5 are always unsupported per SAE J2534-2 clause 23, and
/// so is any pin outside the 1-16 range (`InputPtr` is documented as "the pin
/// number, 1 to 16") -- each rejected with `PDU_ERR_MUX_RSC_NOT_SUPPORTED`.
/// Covers the mock's own boundary (`j2534-0404-mock/src/lib.rs`'s
/// `0 | 4 | 5 => ...`, `17.. => ...` arms), not just the two named-unsupported
/// pins -- an edge-case-hunter follow-up on this same PR (the service itself
/// does no pin-range validation by design, relying entirely on the native/
/// mock layer, so this boundary is otherwise unverified above the mock).
#[tokio::test]
#[serial]
async fn read_j1962_pin_voltage_rejects_pins_4_and_5() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let read_pin_voltage_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_J1962PIN_VOLTAGE").await;

    for pin in [0, 4, 5, 17, 1000] {
        let status = read_j1962_pin_voltage(&mut client, read_pin_voltage_id, pin)
            .await
            .expect_err(&format!(
                "PDU_IOCTL_READ_J1962PIN_VOLTAGE should reject pin {pin}"
            ));
        let detail =
            error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
        assert_eq!(
            detail.pdu_error,
            PduError::PduErrMuxRscNotSupported as i32,
            "pin {pin} should be rejected with PDU_ERR_MUX_RSC_NOT_SUPPORTED"
        );
    }

    server.shutdown().await;
}

/// ADR-185 Stage 2 (mock-fidelity fix): pin 4 is now rejected by the
/// Discovery-cache fail-fast layer (`enforce_discovery_capability`'s
/// `DeviceFlag` check against `DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED`),
/// not just the pre-existing native `ERR_PIN_INVALID` path --
/// `read_j1962_pin_voltage_rejects_pins_4_and_5` above already proves the
/// `PduError` (`PDU_ERR_MUX_RSC_NOT_SUPPORTED`) is unchanged either way (ADR-185
/// Decision 5: the `PduError` class must not depend on early-vs-late
/// rejection), but the two paths ARE distinguishable at the outer gRPC `Code`
/// layer specifically because Decision 5's equivalence guarantee stops at
/// `PduError`, not the outer `Code` -- every Discovery-driven rejection goes
/// through `state_guard_status`/`Code::FailedPrecondition` uniformly
/// (`enforce_discovery_capability`), while the native `ERR_PIN_INVALID` path
/// goes through `map_native_error_as`, which reports the blanket
/// `Code::Internal` for every `PduError` but `PduErrInvalidHandle`
/// (`error.rs`). Only the mock's own per-pin-aware
/// `DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED` fix (this same change) makes
/// this observable -- before it, the mock unconditionally reported
/// `Supported = 1`, so the Discovery precheck never rejected pin 4/5 at all
/// and every rejection necessarily came from the native path.
#[tokio::test]
#[serial]
async fn read_j1962_pin_voltage_pin_4_is_rejected_by_the_discovery_fail_fast_path() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let read_pin_voltage_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_J1962PIN_VOLTAGE").await;

    let status = read_j1962_pin_voltage(&mut client, read_pin_voltage_id, 4)
        .await
        .expect_err("PDU_IOCTL_READ_J1962PIN_VOLTAGE should reject pin 4");
    assert_eq!(
        status.code(),
        tonic::Code::FailedPrecondition,
        "pin 4 must now be rejected by the ADR-185 Discovery-cache fail-fast layer \
         (Code::FailedPrecondition), not fall through to the native ERR_PIN_INVALID path \
         (which would report Code::Internal)"
    );
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrMuxRscNotSupported as i32,
        "the PduError itself must be unchanged from the native path (ADR-185 Decision 5)"
    );

    server.shutdown().await;
}

/// (d) Missing/malformed `input_data` is rejected with `InvalidArgument`.
#[tokio::test]
#[serial]
async fn read_j1962_pin_voltage_rejects_missing_input_data() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let read_pin_voltage_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_J1962PIN_VOLTAGE").await;

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                read_pin_voltage_id,
            )),
            input_data: None,
            has_output: true,
        })
        .await
        .expect_err("PDU_IOCTL_READ_J1962PIN_VOLTAGE with no input_data should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// (e) Module not connected is rejected with `PDU_ERR_MODULE_NOT_CONNECTED`,
/// mirroring `module_scoped_ioctls_reject_when_module_not_connected`'s
/// pattern for `PDU_IOCTL_READ_PROG_VOLTAGE`/`PDU_IOCTL_READ_VBATT` above.
/// Deliberately uses the plain, non-J2534-2-opted-in `TestServer::start()`
/// (unlike the tests above) -- `require_connected_device_for`'s connection
/// check runs before the J2534-2 opt-in gate, so this test's assertion is
/// unaffected by the module's `pname` either way.
#[tokio::test]
#[serial]
async fn read_j1962_pin_voltage_rejects_module_not_connected() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let read_pin_voltage_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_J1962PIN_VOLTAGE").await;

    let status = read_j1962_pin_voltage(&mut client, read_pin_voltage_id, 1)
        .await
        .expect_err("PDU_IOCTL_READ_J1962PIN_VOLTAGE should be rejected before ModuleConnect");
    assert_eq!(
        status.code(),
        tonic::Code::FailedPrecondition,
        "PDU_IOCTL_READ_J1962PIN_VOLTAGE should reject with FailedPrecondition when the module \
         isn't connected"
    );
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrModuleNotConnected as i32,
        "PDU_IOCTL_READ_J1962PIN_VOLTAGE should carry PDU_ERR_MODULE_NOT_CONNECTED"
    );

    server.shutdown().await;
}

/// (f) A native `IOCTL_READ_J1962PIN_VOLTAGE` failure with any status OTHER
/// than `ERR_PIN_INVALID` falls through to the generic
/// `map_native_error_for_link` mapping -- exercised via the
/// `__mock_set_j1962_pin_voltage_error` backdoor, since the pin-based checks
/// alone can only ever produce `ERR_PIN_INVALID` and can never reach this
/// branch. An edge-case-hunter follow-up on this same PR: mirrors
/// `set_prog_voltage_generic_native_failure_still_maps_to_voltage_not_supported`'s
/// role for `PDU_IOCTL_SET_PROG_VOLTAGE` above.
#[tokio::test]
#[serial]
async fn read_j1962_pin_voltage_generic_native_failure_uses_generic_mapping() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let read_pin_voltage_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_J1962PIN_VOLTAGE").await;

    // ERR_FAILED (0x01) -- not ERR_PIN_INVALID -- must fall through to the
    // generic native-error mapping rather than PDU_ERR_MUX_RSC_NOT_SUPPORTED.
    server.backdoor.set_j1962_pin_voltage_error(Some(1));

    let status = read_j1962_pin_voltage(&mut client, read_pin_voltage_id, 1)
        .await
        .expect_err(
            "PDU_IOCTL_READ_J1962PIN_VOLTAGE should fail when the native call is forced to fail",
        );
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_ne!(
        detail.pdu_error,
        PduError::PduErrMuxRscNotSupported as i32,
        "a generic native failure (not ERR_PIN_INVALID) must not map to \
         PDU_ERR_MUX_RSC_NOT_SUPPORTED"
    );

    server.backdoor.set_j1962_pin_voltage_error(None);
    server.shutdown().await;
}

/// (g) `PDU_IOCTL_READ_J1962PIN_VOLTAGE` is a SAE J2534-2 clause 23 feature
/// and must be rejected with `PDU_ERR_ID_NOT_SUPPORTED` on a module that has
/// not opted into SAE J2534-2 (clause 5's `"J2534-2:"` `pname` prefix),
/// mirroring `repeat_message.rs`'s
/// `start_is_rejected_on_a_module_not_opted_into_j2534_2` for
/// `PDU_IOCTL_START_REPEAT_MESSAGE`. Codex review, PR #48: this gate was
/// originally missing entirely, letting a base J2534-1 module reach a
/// clause-23-only extension and get a successful (mock) voltage reading.
#[tokio::test]
#[serial]
async fn read_j1962_pin_voltage_rejects_module_not_opted_into_j2534_2() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let read_pin_voltage_id =
        resolve_ioctl_id(&mut client, "PDU_IOCTL_READ_J1962PIN_VOLTAGE").await;

    let status = read_j1962_pin_voltage(&mut client, read_pin_voltage_id, 1)
        .await
        .expect_err(
            "PDU_IOCTL_READ_J1962PIN_VOLTAGE should be rejected on a module that has not opted \
             into SAE J2534-2 (clause 5)",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"),
        "{}",
        status.message()
    );

    server.shutdown().await;
}

// ── SAE J2534-2 clause 18 Device Configuration (ADR-176/Phase 14) ─────────

/// Packs `(parameter_id, value)` pairs into ADR-178's Device Configuration
/// byte layout (`u32 entry_count`, then `entry_count` × `{u32 parameter_id,
/// u32 value}`, little-endian) and wraps it as `bytearray_data`
/// (`IOBytearray`) -- the replacement carrier for the removed
/// `IODeviceConfigList` proto message.
fn device_config_list_input(entries: &[(u32, u32)]) -> DataItem {
    let mut bytes = Vec::with_capacity(4 + entries.len() * 8);
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for &(parameter_id, value) in entries {
        bytes.extend_from_slice(&parameter_id.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray { data: bytes })),
    }
}

/// Inverse of [`device_config_list_input`]'s packing, for parsing a
/// `PDU_IOCTL_GET_DEVICE_CONFIG` response. A small local duplicate of
/// `rpc_misc.rs`'s private `unpack_device_config_entries` (not reachable
/// from this integration test binary) -- kept intentionally minimal (no
/// malformed-payload validation) since every production-side validation path
/// is exercised directly against the server's error responses, not by
/// re-parsing here.
fn parse_device_config_list_output(bytes: &[u8]) -> Vec<(u32, u32)> {
    assert!(
        bytes.len() >= 4,
        "device config response payload must contain at least a 4-byte entry_count, got {} bytes",
        bytes.len()
    );
    let entry_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    assert_eq!(
        bytes.len(),
        4 + entry_count * 8,
        "device config response payload length should match entry_count"
    );
    bytes[4..]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|chunk| {
            let parameter_id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let value = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            (parameter_id, value)
        })
        .collect()
}

async fn set_device_config(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cmd_id: u32,
    entries: &[(u32, u32)],
) -> Result<(), tonic::Status> {
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data: Some(device_config_list_input(entries)),
            has_output: false,
        })
        .await
        .map(|_| ())
}

async fn get_device_config(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cmd_id: u32,
    parameter_ids: &[u32],
) -> Result<Vec<(u32, u32)>, tonic::Status> {
    let entries: Vec<(u32, u32)> = parameter_ids.iter().map(|&id| (id, 0)).collect();
    let output = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data: Some(device_config_list_input(&entries)),
            has_output: true,
        })
        .await?
        .into_inner()
        .output_data;
    match output.and_then(|d| d.data) {
        Some(data_item::Data::BytearrayData(io_bytearray)) => {
            Ok(parse_device_config_list_output(&io_bytearray.data))
        }
        other => {
            panic!("PDU_IOCTL_GET_DEVICE_CONFIG should return bytearray_data, got {other:?}")
        }
    }
}

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// required for `PDU_IOCTL_GET_DEVICE_CONFIG`/`PDU_IOCTL_SET_DEVICE_CONFIG`.
/// Mirrors `start_j2534_2_server` above (module_handle 1, matching
/// `MOCK_MODULE_HANDLE`).
async fn start_j2534_2_server_for_device_config() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

/// (a) `PDU_IOCTL_SET_DEVICE_CONFIG` followed by a separate
/// `PDU_IOCTL_GET_DEVICE_CONFIG` call round-trips the value on the same
/// `NON_VOLATILE_STORE_x` slot, proving the value is genuinely stored (not an
/// in-request echo) since the GET is a wholly separate `IoCtl` call.
#[tokio::test]
#[serial]
async fn device_config_set_then_get_round_trips_a_single_slot() {
    let server = start_j2534_2_server_for_device_config().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let set_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_DEVICE_CONFIG").await;
    let get_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_DEVICE_CONFIG").await;

    let parameter_id = j2534_0404_sys::bindings::CONFIG_NON_VOLATILE_STORE_1;
    set_device_config(&mut client, set_id, &[(parameter_id, 0x1234_5678)])
        .await
        .expect("PDU_IOCTL_SET_DEVICE_CONFIG should succeed");

    let entries = get_device_config(&mut client, get_id, &[parameter_id])
        .await
        .expect("PDU_IOCTL_GET_DEVICE_CONFIG should succeed");
    assert_eq!(
        entries,
        vec![(parameter_id, 0x1234_5678)],
        "the value written by SET_DEVICE_CONFIG should round-trip through a separate \
         GET_DEVICE_CONFIG call"
    );

    server.shutdown().await;
}

/// (b) A single batched `IoCtl` call can set (and read back) multiple
/// `NON_VOLATILE_STORE_x` slots at once, proving the array-of-pairs design
/// actually batches rather than only supporting one entry per call.
#[tokio::test]
#[serial]
async fn device_config_batched_call_sets_and_gets_multiple_slots() {
    let server = start_j2534_2_server_for_device_config().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let set_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_DEVICE_CONFIG").await;
    let get_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_DEVICE_CONFIG").await;

    let p1 = j2534_0404_sys::bindings::CONFIG_NON_VOLATILE_STORE_2;
    let p2 = j2534_0404_sys::bindings::CONFIG_NON_VOLATILE_STORE_10;
    set_device_config(&mut client, set_id, &[(p1, 0xAAAA_AAAA), (p2, 0xBBBB_BBBB)])
        .await
        .expect("batched PDU_IOCTL_SET_DEVICE_CONFIG should succeed");

    let entries = get_device_config(&mut client, get_id, &[p1, p2])
        .await
        .expect("batched PDU_IOCTL_GET_DEVICE_CONFIG should succeed");
    assert_eq!(
        entries,
        vec![(p1, 0xAAAA_AAAA), (p2, 0xBBBB_BBBB)],
        "a single batched call should set and read back both slots"
    );

    server.shutdown().await;
}

/// An empty `entries` list is accepted as a no-op on both commands (ADR-176
/// Decision: matching native `SCONFIG_LIST`'s own well-defined
/// `NumOfParams: 0` case) -- confirmed here rather than just asserted in the
/// ADR, since an empty list's `ConfigPtr` is a dangling-but-non-null pointer
/// the mock's own `list.ConfigPtr.is_null()` guard must not misclassify as
/// the null-pointer error case.
#[tokio::test]
#[serial]
async fn device_config_empty_entries_list_is_a_noop() {
    let server = start_j2534_2_server_for_device_config().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let set_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_DEVICE_CONFIG").await;
    let get_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_DEVICE_CONFIG").await;

    set_device_config(&mut client, set_id, &[])
        .await
        .expect("PDU_IOCTL_SET_DEVICE_CONFIG with an empty entries list should be a no-op");

    let entries = get_device_config(&mut client, get_id, &[])
        .await
        .expect("PDU_IOCTL_GET_DEVICE_CONFIG with an empty entries list should be a no-op");
    assert_eq!(
        entries,
        Vec::<(u32, u32)>::new(),
        "an empty parameter_id list should return an empty entries list, not an error"
    );

    server.shutdown().await;
}

/// (c) A `parameter_id` outside `NON_VOLATILE_STORE_1..10` is rejected by the
/// native/mock layer's `ERR_INVALID_IOCTL_PARAM_ID`, which
/// `j2534-0404-service`'s `pdu_error_for` maps to `PDU_ERR_ID_NOT_SUPPORTED`
/// (its own arm, alongside `ERR_INVALID_IOCTL_ID`/`ERR_INVALID_PROTOCOL_ID`'s
/// existing one -- `edge-case-hunter` finding, this phase: originally
/// unmapped and fell through to the generic `PDU_ERR_FCT_FAILED` catch-all)
/// with gRPC `Code::Internal` (every `PduError` but `PduErrInvalidHandle`
/// keeps that blanket code, `map_native_error_as`) -- per ADR-176, there is
/// no service-layer allowlist rejecting this earlier.
#[tokio::test]
#[serial]
async fn device_config_out_of_range_parameter_id_is_rejected() {
    let server = start_j2534_2_server_for_device_config().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let set_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_DEVICE_CONFIG").await;
    let get_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_DEVICE_CONFIG").await;

    // 0xC00B is one past CONFIG_NON_VOLATILE_STORE_10 (0xC00A) -- not a real
    // NON_VOLATILE_STORE_x slot.
    let bogus_parameter_id: u32 = 0xC00B;

    let status = set_device_config(&mut client, set_id, &[(bogus_parameter_id, 1)])
        .await
        .expect_err("PDU_IOCTL_SET_DEVICE_CONFIG should reject an out-of-range parameter_id");
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(status.code(), tonic::Code::Internal);
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrIdNotSupported as i32,
        "an out-of-range parameter_id should map to PDU_ERR_ID_NOT_SUPPORTED, the same category \
         as ERR_INVALID_IOCTL_ID/ERR_INVALID_PROTOCOL_ID (error.rs::pdu_error_for)"
    );

    let status = get_device_config(&mut client, get_id, &[bogus_parameter_id])
        .await
        .expect_err("PDU_IOCTL_GET_DEVICE_CONFIG should reject an out-of-range parameter_id");
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(status.code(), tonic::Code::Internal);
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrIdNotSupported as i32,
        "an out-of-range parameter_id should map to PDU_ERR_ID_NOT_SUPPORTED, the same category \
         as ERR_INVALID_IOCTL_ID/ERR_INVALID_PROTOCOL_ID (error.rs::pdu_error_for)"
    );

    server.shutdown().await;
}

/// (d) `PDU_IOCTL_GET_DEVICE_CONFIG`/`PDU_IOCTL_SET_DEVICE_CONFIG` are SAE
/// J2534-2 clause 18 features and must be rejected with
/// `PDU_ERR_ID_NOT_SUPPORTED` on a module that has not opted into SAE
/// J2534-2 (clause 5's `"J2534-2:"` `pname` prefix), mirroring
/// `read_j1962_pin_voltage_rejects_module_not_opted_into_j2534_2` above.
#[tokio::test]
#[serial]
async fn device_config_rejects_module_not_opted_into_j2534_2() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let set_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_DEVICE_CONFIG").await;
    let get_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_DEVICE_CONFIG").await;
    let parameter_id = j2534_0404_sys::bindings::CONFIG_NON_VOLATILE_STORE_1;

    let status = set_device_config(&mut client, set_id, &[(parameter_id, 1)])
        .await
        .expect_err(
            "PDU_IOCTL_SET_DEVICE_CONFIG should be rejected on a module that has not opted into \
             SAE J2534-2 (clause 5)",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"),
        "{}",
        status.message()
    );

    let status = get_device_config(&mut client, get_id, &[parameter_id])
        .await
        .expect_err(
            "PDU_IOCTL_GET_DEVICE_CONFIG should be rejected on a module that has not opted into \
             SAE J2534-2 (clause 5)",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"),
        "{}",
        status.message()
    );

    server.shutdown().await;
}

/// (e) A truncated/malformed `bytearray_data` payload -- ADR-178's Decision
/// requires the unpack step to reject this rather than misparse it, since
/// the proto no longer enforces framing for this feature the way the removed
/// `IODeviceConfigList` message did. Covers both the too-short-for-even-the-
/// count case and the entry_count-doesn't-match-remaining-bytes case, for
/// both commands.
#[tokio::test]
#[serial]
async fn device_config_malformed_bytearray_payload_is_rejected() {
    let server = start_j2534_2_server_for_device_config().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let set_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_DEVICE_CONFIG").await;
    let get_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_DEVICE_CONFIG").await;

    // Too short to even contain the 4-byte entry_count.
    let too_short = vec![0x01, 0x02, 0x03];
    // entry_count = 1 claims one 8-byte entry follows, but only 4 bytes do.
    let mut count_mismatch = 1u32.to_le_bytes().to_vec();
    count_mismatch.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

    for malformed in [too_short, count_mismatch] {
        let input = DataItem {
            data: Some(data_item::Data::BytearrayData(IoBytearray {
                data: malformed.clone(),
            })),
        };

        let status = client
            .io_ctl(IoCtlRequest {
                handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                })),
                io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(set_id)),
                input_data: Some(input.clone()),
                has_output: false,
            })
            .await
            .expect_err(
                "PDU_IOCTL_SET_DEVICE_CONFIG should reject a malformed bytearray_data payload",
            );
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{malformed:?}");

        let status = client
            .io_ctl(IoCtlRequest {
                handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                })),
                io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(get_id)),
                input_data: Some(input),
                has_output: true,
            })
            .await
            .expect_err(
                "PDU_IOCTL_GET_DEVICE_CONFIG should reject a malformed bytearray_data payload",
            );
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{malformed:?}");
    }

    server.shutdown().await;
}

/// (f) The wrong `DataItem` variant (e.g. `unum32_value` instead of
/// `bytearray_data`) is rejected with a clear `INVALID_ARGUMENT`, for both
/// commands.
#[tokio::test]
#[serial]
async fn device_config_wrong_data_item_variant_is_rejected() {
    let server = start_j2534_2_server_for_device_config().await;
    let mut client = server.client().await;

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
        })
        .await
        .expect("module_connect should succeed");

    let set_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_DEVICE_CONFIG").await;
    let get_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_DEVICE_CONFIG").await;

    let wrong_variant = DataItem {
        data: Some(data_item::Data::Unum32Value(42)),
    };

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(set_id)),
            input_data: Some(wrong_variant.clone()),
            has_output: false,
        })
        .await
        .expect_err(
            "PDU_IOCTL_SET_DEVICE_CONFIG should reject a non-bytearray_data DataItem variant",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("bytearray_data"),
        "{}",
        status.message()
    );

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::ModuleHandle(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            })),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(get_id)),
            input_data: Some(wrong_variant),
            has_output: true,
        })
        .await
        .expect_err(
            "PDU_IOCTL_GET_DEVICE_CONFIG should reject a non-bytearray_data DataItem variant",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("bytearray_data"),
        "{}",
        status.message()
    );

    server.shutdown().await;
}

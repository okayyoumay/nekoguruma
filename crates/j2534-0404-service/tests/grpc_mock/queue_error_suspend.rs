//! `CP_SuspendQueueOnError` (ADR-147): a third `LogicalLinkState::tx_suspended()`
//! source, alongside `PDU_IOCTL_SUSPEND_TX_QUEUE` and a sibling CLL's held
//! `LOCK_PHYSICAL_TX_QUEUE` (ADR-123). Content-triggered (a timed-out COP, or
//! a `0x7F`-led negative response the RC engine (`rc_handling.rs`) was never
//! asked to auto-handle), transmits-only (lock-class) gating, and four
//! explicit escapes: `PDU_IOCTL_RESUME_TX_QUEUE`, a `CoptUpdateparam`
//! promoting Active `CP_SuspendQueueOnError` to `0`, a later positive
//! response, and (implicitly, ADR-147) a hard channel error going offline.

use serial_test::serial;
use vci_service_interface::{
    CancelComPrimitiveRequest, ComLogicalLinkHandle, ComPrimitiveCtrlData, ComPrimitiveHandle,
    ConnectComLogicalLinkRequest, DisconnectComLogicalLinkRequest, EventNotification,
    ExpectedResponseData, GetStatusRequest, PduComPrimitiveStatus, StartComPrimitiveRequest,
    SubscribeEventRequest, get_status_request, status_response,
    vci_service_client::VciServiceClient,
};

use crate::harness::*;

const CP_SUSPEND_QUEUE_ON_ERROR: u32 = 0x802A;
const CP_RC_BYTE_OFFSET: u32 = 0x8028;
const CP_RC78_HANDLING: u32 = 0x8027;
const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id`
/// (mirrors `locks_and_param_classes.rs`'s identical helper, this suite's
/// per-file-duplication convention).
async fn resolve_ioctl_id(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    name: &str,
) -> u32 {
    client
        .get_object_id(vci_service_interface::GetObjectIdRequest {
            object_type: vci_service_interface::ObjectType::ObjtIoCtrl as i32,
            shortname: name.to_string(),
        })
        .await
        .unwrap_or_else(|err| panic!("get_object_id({name}) should succeed: {err}"))
        .into_inner()
        .pdu_object_id
}

/// Issues a CLL-targeted `IoCtl` with no input/output payload (mirrors
/// `locks_and_param_classes.rs`'s identical helper).
async fn io_ctl_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    cmd_id: u32,
) -> Result<(), tonic::Status> {
    client
        .io_ctl(vci_service_interface::IoCtlRequest {
            handle: Some(vci_service_interface::io_ctl_request::Handle::CllHandle(
                cll_handle,
            )),
            io_ctrl_command: Some(
                vci_service_interface::io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id),
            ),
            input_data: None,
            has_output: false,
        })
        .await
        .map(|_| ())
}

/// Starts a fire-and-forget `CoptSendrecv` (no response expected, single
/// send/no receive cycle) -- mirrors `locks_and_param_classes.rs`'s identical
/// helper. Used for the plain "is this transmitting item held or dispatched"
/// probes.
async fn queue_send_recv(
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

/// Starts a `CoptSendrecv` expecting a positive `0x62`-prefixed response
/// (single send/receive cycle) -- the "real" request whose timeout/negative
/// response triggers `CP_SuspendQueueOnError` in the tests below. Mirrors
/// `rc_handling.rs`'s identical helper.
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
        .expect("start_com_primitive should succeed");
}

async fn subscribe(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
) -> tonic::Streaming<EventNotification> {
    client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(
                vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_handle),
            ),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner()
}

fn is_rx_timeout(item: &vci_service_interface::EventItem) -> bool {
    matches!(
        item.data,
        Some(vci_service_interface::event_item::Data::ErrorData(error))
            if error == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32
    )
}

fn is_finished(item: &vci_service_interface::EventItem) -> bool {
    matches!(
        item.data,
        Some(vci_service_interface::event_item::Data::CopStatus(status))
            if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
    )
}

/// Mirrors `cop_ctrl_cycles.rs`'s identical helper.
fn is_cancelled(item: &vci_service_interface::EventItem) -> bool {
    matches!(
        item.data,
        Some(vci_service_interface::event_item::Data::CopStatus(status))
            if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
    )
}

/// `GetStatus(cop_handle)`, unwrapped to the bare `PduComPrimitiveStatus` --
/// mirrors `cop_ctrl_cycles.rs`'s identical helper (this suite's
/// per-file-duplication convention).
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

/// A UDS negative response (`0x7F <SID> <NRC>`) for request SID `0x22`, on
/// CAN ID `0x7E8` -- mirrors `rc_handling.rs`'s identical helper.
fn negative_response_can(nrc: u8) -> Vec<u8> {
    can_frame(0x7E8, &[0x7F, 0x22, nrc])
}

/// A UDS positive response (`0x62 ...`) on CAN ID `0x7E8`.
fn positive_response_can() -> Vec<u8> {
    can_frame(0x7E8, &[0x62, 0xF1, 0x90])
}

/// Connects a CAN-family (ISO15765) CLL addressed to a single ECU
/// (`CP_CanPhysReqId = 0x7E0` / `CP_CanRespUSDTId = 0x7E8`), with the given
/// extra ComParams applied before connect.
async fn connect_can_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    extra_params: &[(u32, u32)],
) -> ComLogicalLinkHandle {
    let mut params = vec![(j2534_0404::DATA_RATE, 500_000)];
    params.extend_from_slice(extra_params);
    let cll_handle = create_and_connect_cll(client, j2534_0404::ISO15765, &params).await;
    set_unique_resp_table_and_promote(
        client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
            ],
        )],
    )
    .await;
    cll_handle
}

/// Bullet 1 + 7: a COP that times out (`PduErrEvtRxTimeout`) suspends the
/// queue when `CP_SuspendQueueOnError = 1` -- a subsequently-queued
/// transmitting COP is held (not written) until the client explicitly
/// issues `PDU_IOCTL_RESUME_TX_QUEUE`.
#[tokio::test]
#[serial]
async fn timeout_suspends_queue_until_explicit_resume() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = connect_can_cll(
        &mut client,
        &[
            (j2534_0404::P2_MAX, 200_000), // 200 ms -- short, no response ever injected
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the first COP should time out (no response was ever injected)"
    );

    // The timeout above must have suspended the queue: a fresh transmitting
    // COP queued now must be held, not written.
    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a COP queued after the timeout must stay held while CP_SuspendQueueOnError = 1"
    );

    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    drop(events);
    server.shutdown().await;
}

/// Bullet 2 + 6: an unmapped-NRC (`0x31`, not one of the RC21/RC23/RC78
/// codes at all) `0x7F` response suspends the queue on a KWP-family
/// (ISO14230) CLL, holding two subsequently-queued transmitting COPs in
/// FIFO order until a later positive response (delivered to an independent
/// tier-2/monitor registrant) resumes and drains them.
#[tokio::test]
#[serial]
async fn kwp_unmapped_nrc_suspends_and_drains_fifo_via_positive_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::P2_MAX, 300_000),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // The independent tier-2/monitor registrant is armed FIRST, before
    // anything is ever held: once cop2/cop3 below are siphoned into
    // `tx_held`, a non-transmitting item (like this monitor's own
    // `NumSendCycles == 0` SendRecv) queued behind them would itself be
    // held by the FIFO no-overtake rule (`dispatch_tx_item`) and never
    // reach its own registrant-insertion step. It is armed with a
    // `0x62`-only pattern (NOT `harness::arm_receive_only_monitor`'s fully
    // vacuous one) so it does not itself bind-and-classify-Positive the
    // unmapped-NRC frame below -- a vacuous tier-2 registrant would catch
    // that otherwise-unbound frame too (per this ADR's own "tier-2 only
    // ever classifies Positive" rule), masking the very suspend this test
    // means to prove.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 2,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x62],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv, positive-only monitor) should succeed");

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Standard 4-byte-header (format 0x80) KWP negative response, unmapped
    // NRC 0x31 -- unhandled regardless of any CP_RCxxHandling configuration.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &[0x80, 0x10, 0xF1, 0x03, 0x7F, 0x22, 0x31],
        j2534_0404::ISO14230,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the first COP should eventually finish (its own expected pattern never matched the NRC)"
    );

    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    queue_send_recv(&mut client, cll_handle, vec![0xBB]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "both COPs queued after the unmapped-NRC response must stay held"
    );

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &[0x80, 0x10, 0xF1, 0x03, 0x62, 0xF1, 0x90],
        j2534_0404::ISO14230,
    );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1)[4..],
        [0xAA],
        "the held COPs must drain in FIFO order (first queued, first written)"
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 2)[4..],
        [0xBB]
    );

    drop(events);
    server.shutdown().await;
}

/// Bullet 3 + 9: a `0x7F` with NRC `0x78` but `CP_RC78Handling = 0`
/// (disabled) suspends the queue -- the RC engine was never asked to
/// auto-handle it, so it is unhandled per `RcHandlingConfig::is_unhandled_negative`.
/// Resumed via `CoptUpdateparam` setting Active `CP_SuspendQueueOnError` to
/// `0` while suspended.
#[tokio::test]
#[serial]
async fn rc78_disabled_suspends_and_resumes_via_update_param() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = connect_can_cll(
        &mut client,
        &[
            (j2534_0404::P2_MAX, 300_000),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_RC78_HANDLING, 0),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &negative_response_can(0x78),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the first COP should eventually finish"
    );

    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a 0x78 NRC with CP_RC78Handling disabled must suspend the queue"
    );

    // Resume via `CoptUpdateparam` landing Active `CP_SuspendQueueOnError =
    // 0` WITHOUT clearing the backlog first: `tx_held` is currently
    // non-empty (the queued, still-held transmitting COP above). This is the
    // real client recovery flow the ADR-147 doc comment promises --
    // `CoptUpdateparam` is the one item kind that can itself clear
    // `tx_suspended_by_error` (via `handle_update_param`'s promotion path),
    // so it must bypass `dispatch_tx_item`'s FIFO no-overtake clause while
    // error-suspended rather than get siphoned behind the COP it exists to
    // release (the deadlock this test used to route around with a manual
    // `PDU_IOCTL_CLEAR_TX_QUEUE`, see git history for the prior version of
    // this test and its own comment documenting the bug).
    set_com_param_unum32(&mut client, cll_handle, CP_SUSPEND_QUEUE_ON_ERROR, 0).await;
    promote_via_update_param(&mut client, cll_handle).await;

    // The promotion must both clear the suspension AND let the
    // ORIGINALLY-HELD transmitting COP (0xAA, still parked in `tx_held`)
    // then dispatch -- no new COP is queued here; the held one is the sole
    // source of the second write.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    drop(events);
    server.shutdown().await;
}

/// Bullet 4: a `0x7F` with NRC `0x78` and `CP_RC78Handling` ENABLED does NOT
/// suspend the queue -- the RC engine claims it (pending-RC), so it is not
/// "unhandled" and there is no regression to `rc_handling.rs`'s existing
/// RC78 auto-handling behavior.
#[tokio::test]
#[serial]
async fn rc78_enabled_does_not_suspend() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = connect_can_cll(
        &mut client,
        &[
            (j2534_0404::P2_MAX, 2_000_000),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_RC78_HANDLING, 1),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &negative_response_can(0x78),
        j2534_0404::ISO15765,
    );
    // Give the RC-handled 0x78 a moment to be processed (it extends the
    // COP's own wait rather than finishing it), then let the real positive
    // response land so the COP finishes normally.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &positive_response_can(),
        j2534_0404::ISO15765,
    );
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "the COP should finish once the real positive response arrives"
    );

    // No suspension: a fresh transmitting COP dispatches immediately.
    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    drop(events);
    server.shutdown().await;
}

/// Bullet 5: `CP_SuspendQueueOnError = 0` (the default -- never set) means
/// neither a timeout nor an unmapped-NRC response triggers any suspension --
/// no behavior change from today.
#[tokio::test]
#[serial]
async fn disabled_by_default_no_suspension() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = connect_can_cll(&mut client, &[(j2534_0404::P2_MAX, 200_000)]).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the first COP should time out (no response was ever injected)"
    );

    // No suspension: dispatches immediately with no held delay.
    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    drop(events);
    server.shutdown().await;
}

/// Bullet 8 (adjusted to match `PDU_IOCTL_CLEAR_TX_QUEUE`'s actual, documented
/// semantics -- see this file's own module-level note in the implementer's
/// report): `PDU_IOCTL_CLEAR_TX_QUEUE` cancels a held item while
/// error-suspended (same as it already does for `tx_suspended_by_ioctl`/
/// `tx_suspended_by_lock`, `pdu_ioctl.rs`), but -- per `cancel_held_tx_items`'s
/// `reset_suspended = false` for this ioctl -- does NOT itself resume
/// dispatch: a COP queued afterward still stays held until a genuine resume
/// (`PDU_IOCTL_RESUME_TX_QUEUE` here) fires.
#[tokio::test]
#[serial]
async fn clear_tx_queue_cancels_held_item_without_resuming() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = connect_can_cll(
        &mut client,
        &[
            (j2534_0404::P2_MAX, 200_000),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the first COP should time out"
    );

    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    let clear_tx_queue_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_CLEAR_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, clear_tx_queue_id)
        .await
        .expect("PDU_IOCTL_CLEAR_TX_QUEUE should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(vci_service_interface::event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "the held COP should be cancelled by PDU_IOCTL_CLEAR_TX_QUEUE"
    );

    // Still suspended: a COP queued after the clear stays held too.
    queue_send_recv(&mut client, cll_handle, vec![0xBB]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "PDU_IOCTL_CLEAR_TX_QUEUE must not itself resume dispatch"
    );

    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    drop(events);
    server.shutdown().await;
}

/// Bullet 10: the error-suspend source composes independently with the
/// existing lock source (ADR-123) -- clearing error-suspend (via
/// `PDU_IOCTL_RESUME_TX_QUEUE`, which per ADR-147 clears `tx_suspended_by_error`
/// too) does not clear a sibling's held `LOCK_PHYSICAL_TX_QUEUE` suspension,
/// and releasing that lock is what finally resumes dispatch.
#[tokio::test]
#[serial]
async fn composes_independently_with_sibling_lock_suspend() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a shares cll_b's physical resource: same service-level protocol
    // AND baud rate, so `find_physical_lock_holder`'s `hw_protocol_id`
    // comparison (or, once both are connected, `channel_key` equality)
    // treats them as the same physical channel (ADR-123) -- a raw `CAN`
    // cll_a would NOT share cll_b's ISO15765 physical resource at all.
    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_b = connect_can_cll(
        &mut client,
        &[
            (j2534_0404::P2_MAX, 200_000),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_b).await;

    start_send_recv(&mut client, cll_b, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    // Waits specifically for `Finished` (not just the earlier `RxTimeout`
    // error event) -- `lock_resource`'s active-transmission conflict check
    // (ADR-044/123) rejects while cll_b's own COP is still executing, and
    // `Finished` is the only event that proves it no longer is.
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "cll_b's first COP should time out and finish, setting tx_suspended_by_error"
    );

    // cll_a's TX-queue lock recomputes cll_b's tx_suspended_by_lock = true
    // too, since it shares the physical resource. `executing_cop`'s own
    // clear (`dispatch_tx_item`, on the poll task) runs a moment after the
    // `Finished` notification above reaches this client (two independent
    // tokio tasks) -- retry briefly rather than assume the ordering.
    let mut lock_attempts = 0;
    loop {
        match client
            .lock_resource(vci_service_interface::LockResourceRequest {
                cll_handle: Some(cll_a),
                lock_mask: LOCK_PHYSICAL_TX_QUEUE,
            })
            .await
        {
            Ok(_) => break,
            Err(err) if lock_attempts < 50 => {
                lock_attempts += 1;
                let _ = err;
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
            Err(err) => panic!("lock_resource should succeed: {err}"),
        }
    }

    queue_send_recv(&mut client, cll_b, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    // Clearing cll_b's error-suspend (and its own, never-set ioctl-suspend)
    // must not clear cll_a's independently-held lock-suspend.
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_b, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "cll_b's queued COP must stay held behind cll_a's LOCK_PHYSICAL_TX_QUEUE even after \
         cll_b's own error-suspend (and ioctl-suspend) clear"
    );

    client
        .unlock_resource(vci_service_interface::UnlockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("unlock_resource should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    drop(events);
    server.shutdown().await;
}

/// Bullet 11: a non-transmitting COP (`CoptUpdateparam`, `TxItem::transmits()
/// == false`) can still execute while error-suspended -- the siphon gating
/// (`dispatch_tx_item`/`drain_tx_held_backlog`) treats `tx_suspended_by_error`
/// like the lock-class source, not the unconditional ioctl-class one.
#[tokio::test]
#[serial]
async fn non_transmitting_update_param_executes_while_error_suspended() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = connect_can_cll(
        &mut client,
        &[
            (j2534_0404::P2_MAX, 200_000),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the first COP should time out, setting tx_suspended_by_error"
    );

    // A CoptUpdateparam that does not touch CP_SuspendQueueOnError (still
    // Active = 1 throughout) must still finish promptly -- it is
    // non-transmitting and tx_held is empty, so it is never siphoned at all.
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
    assert!(
        wait_for_event(&mut events, 1000, is_finished).await,
        "CoptUpdateparam must finish promptly despite the CLL being error-suspended"
    );

    // The suspension itself is still in effect: a transmitting COP queued
    // now must stay held.
    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the transmitting COP must still be held -- only the non-transmitting \
         CoptUpdateparam bypassed the suspension"
    );

    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    drop(events);
    server.shutdown().await;
}

/// Regression for `bind_registrant`/`bind_frame`'s ADR-147 correction, bug 1
/// (design-advisor finding, "tester-present false-suspend"): the OLD code
/// wrote `queue_error_class = Suspend` as a loose side effect the moment ANY
/// tier-1-non-vacuous registrant's own `rc_cfg` classified the frame as an
/// unhandled negative response, even one that never actually binds the
/// frame -- landing BEFORE the later tester-present-discard check ran, and
/// never rolled back when that check fired. Here, an "unrelated" registrant
/// (its own request `cop_data` is empty, so its frozen `rc_cfg.request_sid`
/// is `None` -- unrestricted, so its SID-echo gate never excludes anything)
/// sits waiting on the CLL when the ECU's tester-present NEGATIVE reply
/// arrives. Confirms the reply is bound to tester-present's own discard
/// signature (step 3) and causes NO suspension at all, regardless of what
/// the unrelated registrant's `rc_cfg` made of it during the scan.
#[tokio::test]
#[serial]
async fn tester_present_negative_reply_does_not_suspend_via_unrelated_none_request_sid_registrant()
{
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // A plain (non-ISO15765) CAN CLL: unlike ISO15765, its CoptSendrecv path
    // accepts an empty `cop_data` (the resolved TX message is just the
    // 4-byte CAN ID header, well within CAN's 4..=12 size range) -- the only
    // way to get a registrant whose own `rc_cfg.request_sid` is genuinely
    // `None` (`request_sid: request.cop_data.first().copied()`) while still
    // being a real tier-1 (`NumSendCycles >= 1`) registrant.
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
            ],
        )],
    )
    .await;

    // Arm tester-present with a configured negative-response prefix
    // (`[0x7F, 0x3E]`, the standard `7F <SID>` shape for request SID 0x3E)
    // so the injected reply below binds at step 3 (tester-present discard),
    // mirroring `tester_present_reqrsp.rs`'s
    // `reqrsp_1_discards_negative_response_prefix_match`.
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_EXP_NEG_RESP,
        vec![0x7F, 0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX, 5_000_000).await;
    // ADR-147: enable error-suspend, and a non-default RC-byte-offset so
    // `is_unhandled_negative` actually engages at all (its default,
    // `rc_byte_offset < 2`, always declines to classify).
    set_com_param_unum32(&mut client, cll_handle, CP_SUSPEND_QUEUE_ON_ERROR, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_RC_BYTE_OFFSET, 2).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = subscribe(&mut client, cll_handle).await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("CoptStartcomm should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "CoptStartcomm should finish, arming tester-present"
    );

    // The "unrelated" tier-1 registrant: its own request is empty
    // (`cop_data: vec![]`), so its frozen `rc_cfg.request_sid` is `None` --
    // the exact repro condition (ADR-147 design-advisor finding 1). It waits
    // on an unrelated positive (0x62-prefixed) response that never arrives,
    // so its own expected_response never binds the tester-present reply
    // injected below. `NumReceiveCycles == 1` (not `-1`/created-receive-only)
    // is required to keep it genuinely tier-1 (`RegistrantTier::
    // ActiveSendReceive`) -- only that tier's `bind_registrant` scan reads
    // `rc_cfg`/`is_unhandled_negative` at all (a tier-2 registrant has no
    // `rc_cfg`, so it could never reproduce this bug's repro condition) --
    // but a live tier-1 receive-phase wait also runs entirely inline inside
    // this CLL's single channel-poll task (`dispatch_tx_item` awaits it to
    // completion before dequeuing anything else queued behind it), so it
    // must be cancelled explicitly below before the probe COP can ever be
    // dispatched to prove anything about it.
    let unrelated_cop_handle: ComPrimitiveHandle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 9,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x62],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(unrelated registrant) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    // Two writes precede the injection below, in unspecified relative order:
    // this registrant's own (empty-payload) send, and `CoptStartcomm`'s
    // periodic-mode tester-present arm, which (empirically, unlike mode 1's
    // documented immediate idle-triggered send) also transmits its first
    // `CP_TesterPresentMessage` promptly rather than waiting a full
    // `CP_TesterPresentTime` interval. Waiting for both up front (rather than
    // a hardcoded `1`) ensures the unrelated registrant's own send has
    // actually happened -- and so its registrant is inserted and live -- by
    // the time the frame below is injected.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    // The ECU's tester-present negative reply -- matches the configured
    // ExpNegResp prefix, so it binds at step 3 (tester-present discard).
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x3E, 0x22]),
        j2534_0404::CAN,
    );

    // Unlike the sibling regression tests in this file, this injected frame
    // binds via tester-present's own discard (step 3), not any registrant --
    // there is no COP terminal event to synchronize on via `wait_for_event`.
    // Give the poll task (10 ms tick) time to actually read and classify this
    // frame, while the unrelated registrant is still live for its
    // `Tier1NonVacuous` scan to run against, before proceeding below.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Cancel the still-waiting unrelated registrant now that the frame above
    // has been classified: its own receive-phase wait would otherwise occupy
    // this CLL's single channel-poll task for its full `CP_P2Max` (here 5 s),
    // holding up the probe COP below indefinitely regardless of any
    // suspension outcome -- see this test's `unrelated_cop_handle` field doc
    // comment. `CancelComPrimitive` is reaped on this wait's very next tick
    // (`was_cancelled`, `events.rs`), well before its own `CP_P2Max` deadline,
    // and takes an entirely different exit path than a genuine timeout --
    // this cancellation must never itself trigger the timeout hook's
    // `CP_SuspendQueueOnError` classification (`ReceivePhaseOutcome::
    // Terminal` returns before that check is ever reached).
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(unrelated_cop_handle),
        })
        .await
        .expect("cancel_com_primitive(unrelated registrant) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, is_cancelled).await,
        "the unrelated registrant should end as cancelled, freeing the channel poll task"
    );

    // No suspension: a fresh transmitting COP dispatches immediately -- one
    // MORE write than whatever baseline count was already reached above (not
    // a hardcoded absolute count: see this function's own doc comment on the
    // `wait_for_written_count(2)` call above for why a fixed target would be
    // wrong here).
    let baseline = server.backdoor.written_count(MOCK_CHANNEL_ID);
    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, baseline + 1).await;

    drop(events);
    server.shutdown().await;
}

/// Regression for `bind_registrant`'s ADR-147 correction, bug 2
/// (design-advisor finding, "vacuous-descriptor overwrite"): a registrant
/// with ONLY a vacuous descriptor cannot bind in the `Tier1NonVacuous` scan
/// (the `!e.is_vacuous()` gate), so under the OLD code its own
/// classification was computed there, discarded, and then silently
/// overwritten by the SEPARATE `Tier1Vacuous` scan pass -- which never
/// recomputed `unhandled_negative` for its own accepted match, always
/// writing `Positive`. Confirms a SID-echoing, unmapped-NRC 0x7F response
/// bound only via its own vacuous descriptor still suspends the queue (not
/// incorrectly resumed to `Positive`).
#[tokio::test]
#[serial]
async fn vacuous_descriptor_registrant_still_suspends_on_unmapped_nrc() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::P2_MAX, 2_000_000),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // A registrant whose own `expected_response` is fully vacuous (empty
    // mask/pattern, matches any payload). Its own request SID (0x22, the
    // first byte of `cop_data`) still lets `is_unhandled_negative`'s
    // SID-echo gate pass for the negative response injected below.
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
                    acceptance_id: 5,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(vacuous descriptor) should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Standard 4-byte-header (format 0x80) KWP negative response, unmapped
    // NRC 0x31 -- unhandled regardless of CP_RCxxHandling. Bound only via
    // the vacuous descriptor (Tier1Vacuous, step 4), since the
    // Tier1NonVacuous scan (step 2) has no non-vacuous descriptor to match
    // against.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &[0x80, 0x10, 0xF1, 0x03, 0x7F, 0x22, 0x31],
        j2534_0404::ISO14230,
    );
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "the COP should finish -- its vacuous descriptor matches any payload"
    );

    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a SID-echoing unmapped-NRC 0x7F response bound only via a vacuous descriptor must \
         still suspend the queue, not be overwritten to Positive by the Tier1Vacuous scan pass"
    );

    drop(events);
    server.shutdown().await;
}

/// Regression for `bind_registrant`'s ADR-147 correction, bug 3
/// (design-advisor finding, "tier-2 false-resume"): a tier-2 (Receive Only)
/// registrant has no `rc_cfg` at all (RC handling is tier-1 only), so under
/// the OLD code its `unhandled_negative` local was always `false` --
/// blanket-classifying EVERY tier-2 match `Positive`, including a
/// `0x7F`-led payload, wrongly resuming a suspended queue. Confirms the
/// fix's heuristic (a bound tier-2 match starting with `0x7F` writes NO
/// classification at all) leaves the queue suspended, while an ordinary
/// positive tier-2 match still resumes it normally -- the existing
/// auto-resume path (`kwp_unmapped_nrc_suspends_and_drains_fifo_via_positive_response`),
/// unregressed.
#[tokio::test]
#[serial]
async fn tier2_monitor_negative_match_does_not_resume_but_positive_match_does() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = connect_can_cll(
        &mut client,
        &[
            (j2534_0404::P2_MAX, 200_000),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // Two independent tier-2/monitor registrants, armed BEFORE anything is
    // held (same reasoning as `kwp_unmapped_nrc_suspends_and_drains_fifo_via_positive_response`'s
    // own monitor setup): one matches a 0x7F-led payload (acceptance_id 2),
    // the other a genuine positive 0x62-led payload (acceptance_id 3).
    for (acceptance_id, pattern) in [(2u32, 0x7Fu8), (3u32, 0x62u8)] {
        client
            .start_com_primitive(StartComPrimitiveRequest {
                cop_tag: None,
                cll_handle: Some(cll_handle),
                cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
                cop_data: vec![0x00],
                cop_ctrl_data: Some(ComPrimitiveCtrlData {
                    time: 0,
                    num_send_cycles: 0,
                    num_receive_cycles: -1,
                    temp_param_update: 0,
                    expected_response_array: vec![ExpectedResponseData {
                        response_type: 0,
                        acceptance_id,
                        mask_data: vec![0xFF],
                        pattern_data: vec![pattern],
                        unique_resp_ids: vec![],
                    }],
                    tx_flag: None,
                }),
            })
            .await
            .expect("start_com_primitive(tier-2 monitor) should succeed");
    }

    // Suspend the queue via the existing timeout path.
    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the first COP should time out"
    );

    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the queue should be suspended after the timeout"
    );

    // A 0x7F-led payload bound by the tier-2 monitor must NOT resume the
    // queue (bug 3): the still-held COP above must stay held.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &negative_response_can(0x31), // unmapped NRC, matches [0xFF]/[0x7F]
        j2534_0404::ISO15765,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a tier-2 monitor matching a 0x7F-led payload must not resume the queue"
    );

    // A genuine positive payload bound by the OTHER tier-2 monitor DOES
    // resume the queue -- the existing auto-resume path, unregressed.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &positive_response_can(),
        j2534_0404::ISO15765,
    );
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    drop(events);
    server.shutdown().await;
}

/// NOT a regression test for the `CoptUpdateparam`-recovery deadlock fix --
/// see the reporting note below for why. What this test actually pins:
/// `PDU_IOCTL_SUSPEND_TX_QUEUE` holds a transmitting item and a recovery
/// `CoptUpdateparam` together in `tx_held` (both behind the ioctl's
/// unconditional gate, which stays unconditional regardless of whether
/// error-suspend is also active -- `dispatch_tx_item`'s `recovery_bypass`
/// only ever exempts the FIFO no-overtake clause tied to
/// `tx_suspended_by_lock`/`tx_suspended_by_error`, never the
/// `tx_suspended_by_ioctl` term), error-suspend ALSO triggers while
/// ioctl-suspend is still active (composing the two sources), and then the
/// client's own `PDU_IOCTL_RESUME_TX_QUEUE` clears BOTH sources atomically
/// and fully drains the backlog in the correct (FIFO) order. If a future
/// change ever let `PDU_IOCTL_RESUME_TX_QUEUE` clear only
/// `tx_suspended_by_ioctl` and leave `tx_suspended_by_error` set, the held
/// transmitting front item would strand and this test would fail --
/// that is the regression it actually guards against.
///
/// (Reporting note: per `ioctl_resume_tx_queue`'s own doc comment,
/// `PDU_IOCTL_RESUME_TX_QUEUE` clears BOTH `tx_suspended_by_ioctl` and
/// `tx_suspended_by_error` in the same critical section before sending its
/// one wake -- so by the time `drain_tx_held_backlog` runs here, neither
/// suspension source is still set, and the EXISTING (unmodified) front-pop
/// arm alone drains the whole backlog; this test never reaches the NEW
/// middle-pop fallback, and running it against the fully-reverted
/// pre-deadlock-fix code confirms it passes unchanged -- it provides no
/// regression coverage for `recovery_bypass` or the middle-pop fallback.
/// That fallback's own regression coverage is the rewritten
/// `rc78_disabled_suspends_and_resumes_via_update_param` above, the only
/// deterministically testable piece of the deadlock fix; the middle-pop
/// fallback and the `recompute_lock_tx_suspensions` wake-widening it pairs
/// with are otherwise structural-argument-only -- see this ADR's
/// Consequences section.)
#[tokio::test]
#[serial]
async fn ioctl_suspend_then_error_suspend_drains_fully_via_explicit_resume() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = connect_can_cll(
        &mut client,
        &[
            (j2534_0404::P2_MAX, 200_000),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // A transmitting COP, dispatched and already executing (in its
    // receive-wait) BEFORE ioctl-suspend engages -- ioctl-suspend only gates
    // future siphon checks, not an already-executing item, so this COP is
    // unaffected and keeps running toward its own timeout.
    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    // A second transmitting item, held first (front of tx_held) by
    // ioctl-suspend alone.
    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    // A recovery CoptUpdateparam, queued behind it -- also held: ioctl's
    // unconditional gate does not exempt it (the fix's `recovery_bypass`
    // only ever waives the FIFO no-overtake clause under
    // `tx_suspended_by_error`, never `tx_suspended_by_ioctl`).
    set_com_param_unum32(&mut client, cll_handle, CP_SUSPEND_QUEUE_ON_ERROR, 0).await;
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
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "both the transmitting item and the CoptUpdateparam must stay held while \
         PDU_IOCTL_SUSPEND_TX_QUEUE is active"
    );

    // Error-suspend ALSO triggers now, composing with the still-active
    // ioctl-suspend -- the first COP (dispatched before ioctl-suspend
    // engaged) finally times out.
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the first COP should time out, setting tx_suspended_by_error alongside the still-active \
         ioctl-suspend"
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "still held: composing error-suspend with the still-active ioctl-suspend changes nothing \
         until an explicit resume"
    );

    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    // Both suspension sources clear together; the backlog drains fully,
    // dispatching the previously-held transmitting item (the CoptUpdateparam
    // itself never writes any bytes).
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    drop(events);
    server.shutdown().await;
}

/// Regression (b) guarding against a future "simplification" back to a
/// blanket non-transmitting bypass (which would reintroduce ADR-123 Fix B's
/// comm-state-inversion hazard -- see `dispatch_tx_item`'s own doc comment,
/// "an already-held `CoptStartcomm` + a passing empty `CoptStopcomm`
/// executing out of order would leave comm started when the client asked
/// for stopped"): under error-suspend, a transmitting item held ahead of a
/// later-queued, non-transmitting, NON-`UpdateParam` item (an empty-data
/// `CoptStopcomm`) must still be blocked by the ordinary FIFO no-overtake
/// clause -- only `TxItem::UpdateParam` bypasses it.
///
/// (Deviation from the brief's literal "queue a CoptStartcomm (transmits,
/// gets held) then an empty-data CoptStopcomm": `CoptStopcomm`'s own
/// call-time precondition requires `comm_started == true`
/// (`rpc_primitive.rs`, "comm is not started; CoptStopcomm requires a prior
/// successful CoptStartcomm"), and `comm_started` only becomes `true` once a
/// prior `CoptStartcomm` has actually EXECUTED -- so a still-held,
/// not-yet-executed `CoptStartcomm` can never legally have a `CoptStopcomm`
/// queued behind it; the two preconditions are mutually exclusive. A plain
/// transmitting `CoptSendrecv` item stands in for the "held ahead" role
/// instead -- it exercises the identical `item.transmits() == true` gate
/// `dispatch_tx_item`'s FIFO clause keys on, so the regression coverage is
/// the same.)
#[tokio::test]
#[serial]
async fn delay_does_not_overtake_held_transmitting_item_under_error_suspend() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = connect_can_cll(
        &mut client,
        &[
            (j2534_0404::P2_MAX, 200_000),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the SendRecv COP should time out, setting tx_suspended_by_error"
    );

    // A transmitting item, held first (front of tx_held).
    let held_txn_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0xAA],
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
        .expect("start_com_primitive should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the transmitting COP must be held while error-suspended"
    );

    // A `CoptDelay` (non-transmitting, NOT an UpdateParam), queued behind it
    // -- must NOT overtake. (`CoptDelay` is used here rather than the
    // brief's literal "CoptStartcomm ... then an empty-data CoptStopcomm":
    // `CoptStopcomm` cancels every OTHER queued primitive on the link as
    // part of its own, unrelated semantics (`rpc_primitive.rs`, "Cancel all
    // queued primitives for this link so they do not execute after StopComm
    // is processed"), which would cancel the held transmitting item
    // regardless of FIFO order and so cannot demonstrate this guard; and
    // `CoptStartcomm`/`CoptStopcomm`'s call-time preconditions
    // (`comm_started` must be `false`/`true` respectively) are mutually
    // exclusive, so a not-yet-executed, still-held `CoptStartcomm` can never
    // legally have a `CoptStopcomm` queued behind it either. `CoptDelay`
    // exercises the identical `item.transmits() == false`,
    // not-`TxItem::UpdateParam` gate the FIFO clause keys on, with no such
    // cross-item interaction, so the regression coverage is the same.)
    let delay_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptDelay as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 10,
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
        .expect("cop_handle should be present");

    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    // The held transmitting COP must finish BEFORE the CoptDelay does --
    // confirmed by watching (via each COP's own `cop_handle`-scoped event)
    // for the CoptDelay's Finished event arriving too early.
    let mut delay_finished_early = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if item.cop_handle == Some(delay_handle) && is_finished(item) {
                delay_finished_early = true;
            }
            item.cop_handle == Some(held_txn_handle) && is_finished(item)
        })
        .await,
        "the held transmitting COP should finish once resumed"
    );
    assert!(
        !delay_finished_early,
        "CoptDelay must not overtake the held transmitting COP -- FIFO must still apply to \
         non-UpdateParam items under error-suspend"
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            item.cop_handle == Some(delay_handle) && is_finished(item)
        })
        .await,
        "CoptDelay should finish after the held transmitting COP"
    );

    drop(events);
    server.shutdown().await;
}

/// Regression for `bind_registrant`'s ADR-147 classification fix (Codex PR
/// review finding + design-advisor confirmation): the accepted-match
/// `classification` computation used to branch on `r.rc_cfg.as_ref()` being
/// `Some`/`None` FIRST, and only decline to classify (write nothing) for the
/// `None` (tier-2) case -- so a tier-1 registrant WITH an `rc_cfg` present
/// always resolved to a binary `Suspend`/`Positive`, even when
/// `is_unhandled_negative` returned `false` for a reason other than
/// "genuinely not a negative response" (e.g. the NRC belongs to a DIFFERENT
/// request that happens to bind this registrant via a broad/vacuous
/// descriptor, since binding is shape-based, not SID-gated). The fix matches
/// `Some(cfg) if cfg.is_unhandled_negative(payload)` directly, so only a
/// CONFIRMED unhandled negative classifies `Suspend`; every other 0x7F-led
/// accepted match -- confirmed-`Some(cfg)` or not, `None` or not -- declines
/// to classify, same as the tier-2 heuristic already covered.
///
/// A single vacuous-descriptor registrant (`NumReceiveCycles = 2`, so it
/// survives past its first accepted match instead of finishing) is bound
/// twice: first by a SID-echoing unmapped-NRC negative response (confirms
/// `Suspend`, and re-confirms the vacuous-bind classification fix from the
/// prior correction round), then by a negative response whose SID does NOT
/// match this registrant's own request SID -- still bound (the vacuous
/// descriptor matches any payload shape), but `is_unhandled_negative` can't
/// confirm it as unhandled because it can't attribute it to this
/// registrant's own request. Pre-fix, this second frame's classification
/// wrongly resolved to `Positive` (clearing `tx_suspended_by_error` and
/// draining the held backlog); post-fix, it declines to classify, and the
/// queue stays suspended from the first frame.
#[tokio::test]
#[serial]
async fn wrong_sid_negative_response_does_not_false_resume_via_vacuous_registrant() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = connect_can_cll(
        &mut client,
        &[
            (j2534_0404::P2_MAX, 2_000_000),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // A registrant whose own `expected_response` is fully vacuous (empty
    // mask/pattern, matches any payload), request SID 0x22 (the first byte
    // of `cop_data`), surviving past its first match (`NumReceiveCycles =
    // 2`) so it is still bound and active when the second frame arrives.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x22, 0xF1, 0x90],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 2,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 5,
                    mask_data: vec![],
                    pattern_data: vec![],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(vacuous descriptor) should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Frame 1: SID-echoing (0x22, matches this registrant's own request),
    // unmapped NRC 0x31 -- binds via the vacuous descriptor and classifies
    // `Suspend`.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &negative_response_can(0x31),
        j2534_0404::ISO15765,
    );

    // Frame 2: WRONG SID (0x31, does not match this registrant's own
    // request SID 0x22) -- still binds via the same vacuous descriptor
    // (binding is shape-based, not SID-gated), but cannot be confirmed
    // unhandled-negative because it cannot be attributed to this
    // registrant's own request.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x7F, 0x31, 0x11]),
        j2534_0404::ISO15765,
    );

    // Both frames satisfy the vacuous descriptor, so the registrant reaches
    // its second (and last, `NumReceiveCycles = 2`) match and finishes --
    // used here purely as a synchronization point confirming the server has
    // processed both frames before the probe below.
    assert!(
        wait_for_event(&mut events, 2000, is_finished).await,
        "the COP should finish after its second accepted match"
    );

    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a wrong-SID negative response bound only via a vacuous descriptor must decline to \
         classify, not be misclassified Positive and drain the queue still suspended by frame 1"
    );

    drop(events);
    server.shutdown().await;
}

/// Regression for the Codex-review gate-narrowing fix (design-advisor
/// confirmed): `dispatch_tx_item`'s `recovery_bypass` siphon gate (and
/// `drain_tx_held_backlog`'s identical middle-pop fallback) must key off the
/// `UpdateParam` item's OWN queued `params` snapshot reading
/// `CP_SuspendQueueOnError` disabled, not merely "is an `UpdateParam`" --
/// an unrelated `CoptUpdateparam` whose snapshot still has the policy
/// enabled does nothing to recover the queue and must stay FIFO-blocked
/// behind an older held transmitting item, exactly like any other
/// non-recovery, non-transmitting item (`CoptDelay` etc., see
/// `delay_does_not_overtake_held_transmitting_item_under_error_suspend`
/// above).
///
/// Also pins the documented "bypass reordering" quirk (ADR-147 amendment):
/// once a genuine recovery `CoptUpdateparam` (`U_rec`, queued LATER) bypasses
/// and promotes first, the older non-recovery `CoptUpdateparam` (`U_non`) --
/// unblocked by that promotion -- still promotes afterward via normal FIFO
/// drain, and its own call-time snapshot (which still had
/// `CP_SuspendQueueOnError = 1`) wholesale-overwrites the Active set,
/// re-enabling the policy. This does NOT retroactively re-suspend the queue
/// (only trigger sites ever set `tx_suspended_by_error`), but the policy
/// value itself reads enabled again -- confirmed here behaviorally by
/// triggering a fresh timeout afterward and observing the queue suspend
/// again (it would not, had `U_rec`'s Active `0` stuck).
#[tokio::test]
#[serial]
async fn unrelated_update_param_stays_fifo_blocked_then_recovery_drains_in_order() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = connect_can_cll(
        &mut client,
        &[
            (j2534_0404::P2_MAX, 200_000),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // 1. Error-suspend the CLL via a timeout.
    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the first COP should time out, setting tx_suspended_by_error"
    );

    // 2. T: a transmitting COP, held first (front of tx_held).
    let t_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0xAA],
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
        .expect("start_com_primitive should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "T (transmitting) must be held while error-suspended"
    );

    // 3. U_non: a non-recovery CoptUpdateparam -- its own Working snapshot
    // changes an UNRELATED ComParam (CP_RC78Handling), leaving
    // CP_SuspendQueueOnError at its currently-enabled value of 1.
    set_com_param_unum32(&mut client, cll_handle, CP_RC78_HANDLING, 1).await;
    let u_non_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptUpdateparam, U_non) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    // 4. Discriminating assertion, after a settle window: BOTH T stays held
    // AND U_non has not finished (still Idle, never dispatched). Reverting
    // this fix's predicate (back to matching any UpdateParam regardless of
    // its own snapshot) would let U_non bypass the FIFO clause and finish
    // immediately -- this assertion fails pre-fix, passes post-fix (see the
    // revert-and-rerun proof reported alongside this test).
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "T must still be unwritten/undispatched -- U_non must not have drained it"
    );
    assert_eq!(
        cop_status(&mut client, u_non_handle).await,
        PduComPrimitiveStatus::PduCopstIdle,
        "U_non (a non-recovery UpdateParam) must stay FIFO-blocked behind T, not bypass the \
         suspension -- this is the discriminating assertion for the gate-narrowing fix"
    );

    // 5. U_rec: a genuine recovery CoptUpdateparam, queued after U_non.
    set_com_param_unum32(&mut client, cll_handle, CP_SUSPEND_QUEUE_ON_ERROR, 0).await;
    let u_rec_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptUpdateparam, U_rec) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    // 6. Completion order: U_rec finishes first (its own promotion clears
    // the suspension), then T transmits and finishes, then U_non finishes
    // (drained via normal FIFO once the suspension clears).
    let mut t_finished_early = false;
    let mut u_non_finished_early = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if item.cop_handle == Some(t_handle) && is_finished(item) {
                t_finished_early = true;
            }
            if item.cop_handle == Some(u_non_handle) && is_finished(item) {
                u_non_finished_early = true;
            }
            item.cop_handle == Some(u_rec_handle) && is_finished(item)
        })
        .await,
        "U_rec should finish first, clearing the suspension"
    );
    assert!(
        !t_finished_early,
        "T must not finish before U_rec's recovery promotion"
    );
    assert!(
        !u_non_finished_early,
        "U_non must not finish before U_rec's recovery promotion"
    );

    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if item.cop_handle == Some(u_non_handle) && is_finished(item) {
                u_non_finished_early = true;
            }
            item.cop_handle == Some(t_handle) && is_finished(item)
        })
        .await,
        "T should finish next, once U_rec's promotion drains the backlog"
    );
    assert!(
        !u_non_finished_early,
        "U_non must not finish before T (FIFO order preserved once recovered)"
    );

    assert!(
        wait_for_event(&mut events, 2000, |item| {
            item.cop_handle == Some(u_non_handle) && is_finished(item)
        })
        .await,
        "U_non should finish last, drained via normal FIFO once the suspension clears"
    );

    // 7. Pin the documented "bypass reordering" quirk: U_non's own snapshot
    // (captured before U_rec's) still had CP_SuspendQueueOnError = 1, and it
    // promotes LAST (after U_rec), wholesale-overwriting the Active set --
    // re-enabling the policy. Confirmed behaviorally: a fresh timeout now
    // suspends the queue again (it would NOT, had U_rec's Active `0` stuck).
    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the fresh COP should time out"
    );
    queue_send_recv(&mut client, cll_handle, vec![0xCC]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        3,
        "CP_SuspendQueueOnError must read enabled again in Active -- U_non's later, still-\
         enabled snapshot wholesale-overwrote U_rec's disabling promotion (ADR-147 amendment's \
         documented reordering quirk)"
    );

    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 4).await;

    drop(events);
    server.shutdown().await;
}

/// ADR-147's fail-open fix, connect-time reset: a fresh session must never
/// inherit a prior session's `tx_suspended_by_error`. Error-suspends a CLL
/// via a timeout (no explicit resume issued), disconnects it while still
/// suspended, then reconnects the same `cll_handle` and asserts a freshly
/// queued transmitting COP dispatches immediately -- proving the end-to-end
/// "a fresh session never inherits a prior session's error suspension"
/// invariant holds. This test's own graceful disconnect-before-reconnect
/// flow cannot on its own distinguish `finalize_connected_link`'s
/// connect-time `tx_suspended_by_error = false` reset (`rpc_link.rs`) from
/// `cancel_link_cops`'s own disconnect-time clear, since both run before the
/// reconnect completes -- this test pins the OUTCOME, not which of the two
/// sites produced it. `finalize_connected_link_resets_prior_sessions_error_suspension`
/// (`rpc_link.rs`'s unit test module) isolates the connect-time reset itself
/// deterministically, with no disconnect in the picture at all; see this
/// ADR's connect-time-reset addendum for why the connect-time site is still
/// needed as an independent belt-and-braces guard against the boundary leak
/// the narrowed teardown-site bumps reopen.
///
/// **Note on the third amendment's own new scenario (capture-at-fold
/// sequencing, closing the "first suspend-worthy frame of an episode"
/// race).** No integration test was added reproducing that scenario
/// end-to-end (frame folds `Suspend` while `tx_suspended_by_error` is still
/// `false` -> client observes it -> client issues
/// `PDU_IOCTL_RESUME_TX_QUEUE` -> the SAME pass's own end-of-pass writeback
/// still runs afterward). Forcing that exact interleaving deterministically
/// would require the test's own `PDU_IOCTL_RESUME_TX_QUEUE` RPC call to land
/// on the server, and be fully processed, at a point strictly between
/// `poll_rx_inner`'s per-frame fold (mid-batch) and its end-of-pass
/// writeback (same batch, moments later, with no `.await` a test could hook
/// in between) -- there is no natural preemption point in this harness's
/// single-threaded (per-channel) poll-task structure to pin the client's
/// RPC there rather than before the fold or after the writeback, matching
/// the same class of unforceable-without-a-production-pause-hook timing
/// window `ioctl_suspend_then_error_suspend_drains_fully_via_explicit_resume`'s
/// own doc comment above documents for the deadlock-fix's middle-pop
/// fallback, and ADR-147's own Consequences section documents for the
/// `error_clear_seq`-anchored `Suspend` mechanism's client-reaction race
/// generally.
/// `queue_error_class_to_apply_tests` (`events.rs`) instead pins the pure
/// apply-time decision this scenario reduces to directly:
/// `suspend_applies_when_generation_seq_and_policy_all_match` is the "client
/// never reacted, or reacted before this seq was ever captured" cell, and
/// `suspend_discarded_on_seq_mismatch` is the "client resumed before this
/// pass's writeback ran" cell -- together they cover the decision this
/// scenario would otherwise exercise end-to-end.
///
/// **Note on the fifth amendment's own scenario (split direction-specific
/// anchors, closing the companion-`Positive`-vs-primary-timeout race with a
/// BATCH-READ anchor rather than the fourth amendment's fold-time-anchored
/// unified counter).** Likewise not attempted as an end-to-end integration
/// test, for the identical structural reason: forcing a companion channel's
/// poll pass to fold a `Positive` classification and then pause strictly
/// between that fold and its own end-of-pass writeback, with the PRIMARY
/// channel's independent timeout hook landing (and bumping `error_set_seq`)
/// in that exact window, has no natural preemption point in this harness's
/// independently-ticking, unpaused-timer poll tasks -- the same class of
/// unforceable-without-a-production-pause-hook timing window as every other
/// bullet in this doc comment. `queue_error_class_to_apply_tests`
/// (`events.rs`) instead pins the pure apply-time decision directly:
/// `positive_discarded_when_batch_was_read_before_a_timeout_bump` models the
/// bug scenario's seq arithmetic exactly (a batch's own read-time seq vs. a
/// timeout's later bump, with no fold-time parameter involved at all -- the
/// batch-read anchor makes fold timing irrelevant), and
/// `positive_applies_when_batch_was_read_at_current_error_set_seq` confirms
/// the genuine-recovery direction is not regressed by the new anchor.
#[tokio::test]
#[serial]
async fn reconnect_does_not_inherit_prior_sessions_error_suspension() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = connect_can_cll(
        &mut client,
        &[
            (j2534_0404::P2_MAX, 200_000), // short -- no response ever injected
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the first COP should time out, setting tx_suspended_by_error"
    );

    // Confirm the queue is genuinely error-suspended before disconnecting --
    // otherwise this test would prove nothing.
    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the queue must be error-suspended before disconnect, or this test is vacuous"
    );

    drop(events);

    // Disconnect WITHOUT ever issuing PDU_IOCTL_RESUME_TX_QUEUE or any other
    // explicit resume -- the CLL goes offline still error-suspended.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnecting the error-suspended cll should succeed");

    // Reconnect the same cll_handle -- CreateComLogicalLink was not
    // re-issued, so Working ComParams (DATA_RATE, P2_MAX,
    // CP_SuspendQueueOnError, UniqueRespIdTable) are still in place and get
    // re-promoted to Active by this fresh connect. With no sibling CLL
    // keeping the old physical channel's ref_count above 0, disconnect tore
    // it down entirely, so this reconnect opens a brand-new physical
    // channel -- the mock backend's `next_channel_id` counter (never reset
    // by disconnect, see `j1850_autodetect.rs`'s identical
    // `connect_count()`-as-channel_id convention) means the reconnected
    // session's channel_id is NOT `MOCK_CHANNEL_ID` again.
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("reconnecting the same cll_handle should succeed");
    let reconnected_channel_id = server.backdoor.connect_count() as u32;

    // A fresh transmitting COP on the reconnected session must dispatch
    // immediately: the old session's error-suspension must not have
    // survived the reconnect.
    queue_send_recv(&mut client, cll_handle, vec![0xBB]).await;
    wait_for_written_count(&server, reconnected_channel_id, 1).await;

    server.shutdown().await;
}

/// Codex PR review finding r3680112304 (PR #20): `poll_rx_inner`'s
/// `wrote_suspend` block now publishes `link.tx_suspended_by_error = true`
/// eagerly, in the SAME critical section that captures `entry.suspend_seq`,
/// strictly BEFORE the triggering frame is delivered via
/// `deliver_or_enqueue` -- publish-before-exposure, mirroring the
/// receive-phase timeout hook's own existing set-before-expose invariant.
/// Reacting to the delivered frame with a fresh transmitting COP, issued
/// with no settle sleep the moment this frame's own terminal/timeout event
/// is observed, must see the queue already suspended.
///
/// **Caveat, checked by revert-and-rerun:** this test does NOT actually
/// distinguish pre-fix from post-fix behavior in this harness -- confirmed
/// by temporarily disabling the eager publish and rerunning: it still
/// passes. This crate's `current_thread` `#[tokio::test]` runtime (see this
/// file's `reconnect_does_not_inherit_prior_sessions_error_suspension` doc
/// comment for the identical class of finding on a sibling scenario) never
/// preempts `poll_rx_inner` mid-pass -- `deliver_or_enqueue`'s
/// `mpsc::UnboundedSender::send` and every `logical_links.lock().await` in
/// this span are all non-yielding when uncontended, so the WHOLE pass
/// (delivery through the end-of-pass reconciliation loop) runs as one
/// synchronous burst before any other task, including the one forwarding
/// this frame's notification to the client, ever gets scheduled -- meaning
/// the pre-fix reconciliation-loop write has always already landed by the
/// time a black-box client could react, regardless of this fix. There is no
/// natural preemption point in this harness to force the genuine
/// reactive-dispatch-racing-mid-pass interleaving the fix addresses (that
/// interleaving is only reachable in production, where a companion poll
/// task or the gRPC-handler task run on a genuinely concurrent
/// multi-threaded runtime). This test is kept anyway because it still pins
/// the exact code path and the correct steady-state outcome (as every
/// sibling test in this suite already does via the identical
/// queue-then-check-`written_count` idiom), and would catch a REGRESSION
/// that pushed the publish later still (e.g. behind a genuine `.await`) --
/// just not the specific ordering-within-one-synchronous-pass bug this fix
/// closes. Verified correct instead by code inspection: the write sits two
/// statements before `deliver_or_enqueue` in `poll_rx_inner`, under the same
/// lock guard, with no `.await` between them.
#[tokio::test]
#[serial]
async fn error_suspension_is_published_before_the_triggering_frame_is_delivered() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::P2_MAX, 300_000),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Standard 4-byte-header (format 0x80) KWP negative response, unmapped
    // NRC 0x31 -- unhandled regardless of any CP_RCxxHandling configuration,
    // classifying `Suspend` (mirrors
    // `kwp_unmapped_nrc_suspends_and_drains_fifo_via_positive_response`'s
    // identical trigger frame).
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &[0x80, 0x10, 0xF1, 0x03, 0x7F, 0x22, 0x31],
        j2534_0404::ISO14230,
    );
    assert!(
        wait_for_event(&mut events, 2000, |item| is_rx_timeout(item)
            || is_finished(item))
        .await,
        "the first COP should eventually finish (its own expected pattern never matched the NRC)"
    );

    // Regression probe: react to the just-delivered frame IMMEDIATELY -- no
    // settle sleep -- by queuing a fresh transmitting COP, and assert it
    // does not dispatch. This is the same "queue a COP, then assert
    // written_count" idiom every other test in this suite uses to observe
    // `tx_suspended_by_error` indirectly (no direct field access is
    // available to a black-box gRPC test), but exercised with no settle
    // window at all, right at the point the triggering frame's own event was
    // observed -- pinning that the publish is not deferred past this frame's
    // own delivery.
    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a COP queued immediately upon observing the suspend-triggering frame must stay held -- \
         tx_suspended_by_error must already be published by the time this frame is delivered"
    );

    drop(events);
    server.shutdown().await;
}

/// Codex PR review finding r3680112304 (PR #20): the eager mid-batch publish
/// added to the `wrote_suspend` block must not defeat the end-of-pass
/// reconciliation loop's own same-batch last-frame-wins `Positive` handling.
/// A batch containing an unhandled negative response (classifies `Suspend`,
/// eagerly publishing `tx_suspended_by_error = true` mid-batch) followed, in
/// the SAME batch, by a genuine positive response (classifies `Positive`)
/// must still resume by the end of the pass, with a `TxItem::ResumeWake`
/// observable as the previously-held COP draining without any further
/// external stimulus (no explicit `PDU_IOCTL_RESUME_TX_QUEUE`, unlike
/// `kwp_unmapped_nrc_suspends_and_drains_fifo_via_positive_response`'s
/// separately-batched positive response).
///
/// **Caveat (same class as the preceding test's):** this test also passes
/// unchanged with the eager publish disabled -- it pins the pre-existing
/// end-of-pass reconciliation loop's continued authority over same-batch
/// last-frame-wins `Positive`, not any distinguishing effect of the eager
/// write itself. Kept for that steady-state coverage regardless.
#[tokio::test]
#[serial]
async fn same_batch_positive_response_still_resumes_over_an_eager_mid_batch_suspend() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::P2_MAX, 300_000),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_SUSPEND_QUEUE_ON_ERROR, 1),
        ],
    )
    .await;
    let events = subscribe(&mut client, cll_handle).await;

    // The independent tier-2/monitor registrant, armed FIRST so it exists to
    // bind the positive response below -- mirrors
    // `kwp_unmapped_nrc_suspends_and_drains_fifo_via_positive_response`'s
    // identical setup and its own doc comment for why this must be armed
    // before anything is held.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 2,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x62],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv, positive-only monitor) should succeed");

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Queue a transmitting COP now, before the batch below lands, so its
    // drain (or lack thereof) pins the final resumed state.
    queue_send_recv(&mut client, cll_handle, vec![0xAA]).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the held COP must not have dispatched before the same-batch NRC+positive pair lands"
    );

    // One batch: the unmapped-NRC response (classifies `Suspend`, eagerly
    // publishing `tx_suspended_by_error = true` mid-batch per this fix)
    // immediately followed, with no intervening await, by the genuine
    // positive response the monitor registrant above binds (classifies
    // `Positive`) -- landing in the mock's RX queue together, ahead of the
    // poll task's next `PassThruReadMsgs` call, so both are read in the same
    // batch (mirrors `startcomm_optional_message_tx.rs`'s identical
    // back-to-back `inject_rx` pattern for landing two frames in one
    // window).
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &[0x80, 0x10, 0xF1, 0x03, 0x7F, 0x22, 0x31],
        j2534_0404::ISO14230,
    );
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &[0x80, 0x10, 0xF1, 0x03, 0x62, 0xF1, 0x90],
        j2534_0404::ISO14230,
    );

    // Final state: the end-of-pass reconciliation loop's last-frame-wins
    // `Positive` must have overridden the eager mid-batch `Suspend` publish
    // -- the held COP drains with no explicit `PDU_IOCTL_RESUME_TX_QUEUE`,
    // which is only possible via a `TxItem::ResumeWake` (the flag flipping
    // true -> false is not itself independently observable from a black-box
    // gRPC test, but the drain it gates is).
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1)[4..],
        [0xAA],
        "the previously-held COP must have drained, confirming the resume wake fired"
    );

    drop(events);
    server.shutdown().await;
}

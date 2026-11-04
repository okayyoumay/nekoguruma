//! ADR-419: the worker's tester-present is serialized with the client's
//! primitives as ISO 22900-2:2022 8.1.3 Table 1 requires, its discard window
//! ends at the final answer (a `7F 3E 78` chain reloads it), and a missing or
//! negative answer raises `PduErrEvtTesterPresentError` while sending
//! continues.
//!
//! Every test runs one ISO15765 link (or two on one shared channel) with
//! tester-present `3E 00` addressed to the table entry `0x7E0`/`0x7E8`, an
//! expected positive answer `7E 00` and negative answer `7F 3E`, and answers
//! the tester-present (or does not) by injecting frames on `0x7E8`. Timing
//! bounds follow ADR-149: each is wide against the 10 ms poll tick and only
//! as tight as the behaviour it tells apart needs.

use serial_test::serial;
use vci_service_interface::{
    CancelComPrimitiveRequest, ComLogicalLinkHandle, ComPrimitiveCtrlData, ComPrimitiveHandle,
    DisconnectComLogicalLinkRequest, ErrorData, EventNotification, ExpectedResponseData,
    StartComPrimitiveRequest, SubscribeEventRequest, event_item, subscribe_event_request,
    vci_service_client::VciServiceClient,
};

use crate::harness::*;

const CP_RC_BYTE_OFFSET: u32 = 0x8028;
const CP_RC78_HANDLING: u32 = 0x8027;
const CP_P2_STAR: u32 = 0x8011;

type Client = VciServiceClient<tonic::transport::Channel>;
type Events = tonic::Streaming<EventNotification>;

/// What a test varies in the shared setup. Times are microseconds, as the
/// ComParams are.
#[derive(Clone, Copy)]
struct Cfg {
    time_us: u32,
    p2_max_us: u32,
    req_rsp: u32,
    send_type: u32,
    /// Tester-present addressed functionally (`CP_CanFuncReqId` 0x7DF).
    functional: bool,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            time_us: 5_000_000,
            p2_max_us: 5_000_000,
            req_rsp: 1,
            send_type: 0,
            functional: false,
        }
    }
}

fn is_tester_present_error(item: &vci_service_interface::EventItem) -> bool {
    matches!(
        item.data,
        Some(event_item::Data::ErrorData(ErrorData { error_event: error, .. }))
            if error == vci_service_interface::PduErrorEvent::PduErrEvtTesterPresentError as i32
    )
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

fn is_result_data(item: &vci_service_interface::EventItem) -> bool {
    matches!(item.data, Some(event_item::Data::ResultData(_)))
}

fn ms(n: u64) -> std::time::Duration {
    std::time::Duration::from_millis(n)
}

/// Creates and connects an ISO15765 link on `0x7E0`/`0x7E8`-style addressing
/// (`phys`/`resp` CAN IDs), with the tester-present of `cfg` staged and
/// promoted, response-pending handling on (`CP_RCByteOffset` 2, `CP_P2Star`
/// 1 s) and a subscription to its events. Does not start communication.
async fn setup_link(
    client: &mut Client,
    cfg: Cfg,
    phys: u32,
    resp: u32,
) -> (ComLogicalLinkHandle, Events) {
    let cll = create_and_connect_cll(
        client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, cfg.p2_max_us),
            (CP_RC78_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_P2_STAR, 1_000_000),
        ],
    )
    .await;
    set_unique_resp_table(
        client,
        cll,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, resp),
                unum32_param(CP_CAN_PHYS_REQ_ID, phys),
            ],
        )],
    )
    .await;
    set_com_param_bytes(client, cll, CP_TESTER_PRESENT_MESSAGE, vec![0x3E, 0x00]).await;
    set_com_param_unum32(client, cll, CP_TESTER_PRESENT_TIME, cfg.time_us).await;
    set_com_param_unum32(client, cll, CP_TESTER_PRESENT_HANDLING, 1).await;
    set_com_param_unum32(client, cll, CP_TESTER_PRESENT_REQ_RSP, cfg.req_rsp).await;
    set_com_param_unum32(client, cll, CP_TESTER_PRESENT_SEND_TYPE, cfg.send_type).await;
    set_com_param_bytes(
        client,
        cll,
        CP_TESTER_PRESENT_EXP_POS_RESP,
        vec![0x7E, 0x00],
    )
    .await;
    set_com_param_bytes(
        client,
        cll,
        CP_TESTER_PRESENT_EXP_NEG_RESP,
        vec![0x7F, 0x3E],
    )
    .await;
    if cfg.functional {
        set_com_param_unum32(client, cll, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
        set_com_param_unum32(client, cll, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    }
    promote_via_update_param(client, cll).await;
    let events = subscribe(client, cll).await;
    (cll, events)
}

async fn subscribe(client: &mut Client, cll: ComLogicalLinkHandle) -> Events {
    client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner()
}

async fn start_comm(client: &mut Client, cll: ComLogicalLinkHandle, events: &mut Events) {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("CoptStartcomm should succeed");
    assert!(
        wait_for_event(events, 3000, is_finished).await,
        "CoptStartcomm should finish"
    );
}

/// A physical `22 F1 90` send-receive expecting one `62`-prefixed response
/// (`num_receive_cycles` 1) or, with `-1`, an IS-CYCLIC one.
async fn client_request(
    client: &mut Client,
    cll: ComLogicalLinkHandle,
    num_receive_cycles: i32,
) -> ComPrimitiveHandle {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x22, 0xF1, 0x90],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles,
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
        .expect("start_com_primitive(CoptSendrecv) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
}

fn inject(server: &TestServer, can_id: u32, payload: &[u8]) {
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(can_id, payload),
        j2534_0404::ISO15765,
    );
}

/// Every frame written to the mock channel so far, in order.
fn written(server: &TestServer) -> Vec<Vec<u8>> {
    (0..server.backdoor.written_count(MOCK_CHANNEL_ID))
        .map(|i| server.backdoor.written_data(MOCK_CHANNEL_ID, i))
        .collect()
}

fn frame_bytes(can_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = can_id.to_be_bytes().to_vec();
    v.extend_from_slice(payload);
    v
}

fn tp_frame() -> Vec<u8> {
    frame_bytes(0x7E0, &[0x3E, 0x00])
}

fn request_frame() -> Vec<u8> {
    frame_bytes(0x7E0, &[0x22, 0xF1, 0x90])
}

fn count_of(server: &TestServer, frame: &[u8]) -> usize {
    written(server)
        .iter()
        .filter(|f| f.as_slice() == frame)
        .count()
}

/// Waits (polling) until `done()` holds, or panics with `what` after `timeout`.
async fn wait_until(what: &str, timeout: std::time::Duration, mut done: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while !done() {
        assert!(started.elapsed() < timeout, "timed out waiting for {what}");
        tokio::time::sleep(ms(10)).await;
    }
}

/// Item (1): a tester-present that falls due while a client request waits for
/// its response is sent only after the final response, at the first free
/// tick (its cadence is not restarted by having been held).
#[tokio::test]
#[serial]
async fn tester_present_due_during_a_client_response_wait_is_sent_after_the_final_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cfg = Cfg {
        time_us: 400_000,
        ..Cfg::default()
    };
    let (cll, mut events) = setup_link(&mut client, cfg, 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    inject(&server, 0x7E8, &[0x7E, 0x00]);

    client_request(&mut client, cll, 1).await;
    wait_until("the request to be written", ms(2000), || {
        count_of(&server, &request_frame()) == 1
    })
    .await;
    // Due after 400 ms, held for the whole 1.1 s response wait.
    tokio::time::sleep(ms(1100)).await;
    assert_eq!(
        count_of(&server, &tp_frame()),
        1,
        "no tester-present may go while the client request waits for its response"
    );
    assert!(
        !wait_for_event(&mut events, 50, is_tester_present_error).await,
        "the held tester-present is not an error"
    );

    inject(&server, 0x7E8, &[0x62, 0xF1, 0x90, 0x01]);
    assert!(wait_for_event(&mut events, 2000, is_finished).await);
    let finished_at = std::time::Instant::now();
    wait_until("the held tester-present to be sent", ms(1500), || {
        count_of(&server, &tp_frame()) == 2
    })
    .await;
    assert!(
        finished_at.elapsed() < ms(250),
        "the held tester-present is overdue and must go at the first free tick, not a full \
         interval later (took {:?})",
        finished_at.elapsed()
    );

    drop(events);
    server.shutdown().await;
}

/// A long `7F 22 78` chain keeps the tester-present held for its whole
/// length, several tester-present intervals.
#[tokio::test]
#[serial]
async fn a_response_pending_chain_holds_the_tester_present_for_its_whole_length() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cfg = Cfg {
        time_us: 400_000,
        ..Cfg::default()
    };
    let (cll, mut events) = setup_link(&mut client, cfg, 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    inject(&server, 0x7E8, &[0x7E, 0x00]);

    client_request(&mut client, cll, 1).await;
    wait_until("the request to be written", ms(2000), || {
        count_of(&server, &request_frame()) == 1
    })
    .await;
    for _ in 0..4 {
        tokio::time::sleep(ms(350)).await;
        inject(&server, 0x7E8, &[0x7F, 0x22, 0x78]);
    }
    assert_eq!(
        count_of(&server, &tp_frame()),
        1,
        "a response-pending chain longer than the tester-present interval must not let one go"
    );

    inject(&server, 0x7E8, &[0x62, 0xF1, 0x90, 0x01]);
    assert!(wait_for_event(&mut events, 2000, is_finished).await);
    wait_until("the held tester-present to be sent", ms(1500), || {
        count_of(&server, &tp_frame()) == 2
    })
    .await;

    drop(events);
    server.shutdown().await;
}

/// Item (2): a client request issued while a tester-present is outstanding
/// goes after its answer, and nothing is reported for an answered one.
#[tokio::test]
#[serial]
async fn a_client_request_waits_for_an_outstanding_tester_present_answer() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let (cll, mut events) = setup_link(&mut client, Cfg::default(), 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    client_request(&mut client, cll, 1).await;
    tokio::time::sleep(ms(400)).await;
    assert_eq!(
        count_of(&server, &request_frame()),
        0,
        "the request must wait for the outstanding tester-present's answer"
    );

    inject(&server, 0x7E8, &[0x7E, 0x00]);
    wait_until(
        "the request to be written after the answer",
        ms(1500),
        || count_of(&server, &request_frame()) == 1,
    )
    .await;
    assert_eq!(written(&server)[1], request_frame());

    inject(&server, 0x7E8, &[0x62, 0xF1, 0x90, 0x01]);
    assert!(wait_for_event(&mut events, 2000, is_finished).await);
    assert!(
        !wait_for_event(&mut events, 300, is_tester_present_error).await,
        "an answered tester-present raises no error event"
    );

    drop(events);
    server.shutdown().await;
}

/// Item (3): `7F 3E 78` followed by `7E 00` (here injected back to back, so
/// usually one batch) reaches neither a client monitor nor the error event,
/// and the hold is released by the final answer.
#[tokio::test]
#[serial]
async fn pending_then_final_answer_is_neither_delivered_nor_an_error_and_releases_the_hold() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let (cll, mut events) = setup_link(&mut client, Cfg::default(), 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    // A vacuous receive-only monitor would deliver any frame that step 3 of
    // the binding does not take for the tester-present.
    arm_receive_only_monitor(&mut client, cll).await;
    client_request(&mut client, cll, 1).await;

    inject(&server, 0x7E8, &[0x7F, 0x3E, 0x78]);
    inject(&server, 0x7E8, &[0x7E, 0x00]);

    assert!(
        !wait_for_event(&mut events, 400, |item| is_result_data(item)
            || is_tester_present_error(item))
        .await,
        "neither tester-present answer may be delivered, and none is an error"
    );
    wait_until("the request to be released", ms(1500), || {
        count_of(&server, &request_frame()) == 1
    })
    .await;

    drop(events);
    server.shutdown().await;
}

/// A tester-present that gets no answer ends at its window, raises the error
/// event once per such send, and sending continues.
#[tokio::test]
#[serial]
async fn an_unanswered_tester_present_raises_the_error_event_at_expiry_and_sending_continues() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cfg = Cfg {
        time_us: 800_000,
        p2_max_us: 400_000,
        ..Cfg::default()
    };
    let (cll, mut events) = setup_link(&mut client, cfg, 0x7E0, 0x7E8).await;

    let started = std::time::Instant::now();
    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert!(
        wait_for_event(&mut events, 2000, is_tester_present_error).await,
        "the first unanswered tester-present should raise the event"
    );
    assert!(
        started.elapsed() >= ms(350),
        "the event comes at the end of the window, not at the send (after {:?})",
        started.elapsed()
    );
    // The second send goes on at its interval and gets its own event.
    assert!(wait_for_event(&mut events, 2500, is_tester_present_error).await);
    assert!(count_of(&server, &tp_frame()) >= 2);

    drop(events);
    server.shutdown().await;
}

/// A negative answer (not a pending one) is final: the event is raised at
/// once, the hold is released before the window would have ended, and the
/// frame is not delivered.
#[tokio::test]
#[serial]
async fn a_negative_final_answer_raises_the_event_and_releases_the_hold() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let (cll, mut events) = setup_link(&mut client, Cfg::default(), 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    arm_receive_only_monitor(&mut client, cll).await;

    inject(&server, 0x7E8, &[0x7F, 0x3E, 0x22]);
    let mut delivered = false;
    assert!(
        wait_for_event(&mut events, 1500, |item| {
            delivered |= is_result_data(item);
            is_tester_present_error(item)
        })
        .await,
        "a negative tester-present answer should raise the event"
    );
    assert!(!delivered, "the negative answer must not be delivered");

    client_request(&mut client, cll, 1).await;
    wait_until(
        "the request to go before the 5 s window ends",
        ms(1000),
        || count_of(&server, &request_frame()) == 1,
    )
    .await;

    drop(events);
    server.shutdown().await;
}

/// A positive answer raises no event, also not when its window ends.
#[tokio::test]
#[serial]
async fn a_positive_answer_raises_no_error_event() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cfg = Cfg {
        p2_max_us: 500_000,
        ..Cfg::default()
    };
    let (cll, mut events) = setup_link(&mut client, cfg, 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    inject(&server, 0x7E8, &[0x7E, 0x00]);
    assert!(
        !wait_for_event(&mut events, 900, is_tester_present_error).await,
        "a positive answer must raise no error event, at its arrival or at the window's end"
    );

    drop(events);
    server.shutdown().await;
}

/// A `CoptUpdateparam` that switches the handling off after the send leaves
/// the entry: it still holds the channel until its window ends, and the end
/// is silent.
#[tokio::test]
#[serial]
async fn a_disarm_after_the_send_keeps_the_hold_but_reports_no_missing_answer() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cfg = Cfg {
        p2_max_us: 700_000,
        ..Cfg::default()
    };
    let (cll, mut events) = setup_link(&mut client, cfg, 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let sent_at = std::time::Instant::now();
    set_com_param_unum32(&mut client, cll, CP_TESTER_PRESENT_HANDLING, 0).await;
    promote_via_update_param(&mut client, cll).await;

    client_request(&mut client, cll, 1).await;
    tokio::time::sleep(ms(250)).await;
    assert_eq!(
        count_of(&server, &request_frame()),
        0,
        "the disarmed send's entry still holds the channel"
    );
    wait_until("the request after the window ended", ms(2000), || {
        count_of(&server, &request_frame()) == 1
    })
    .await;
    assert!(sent_at.elapsed() >= ms(600));
    assert!(
        !wait_for_event(&mut events, 300, is_tester_present_error).await,
        "the end of a disarmed send's window is silent"
    );

    drop(events);
    server.shutdown().await;
}

/// `CoptStopcomm` while a send is outstanding: the entry stays and holds,
/// the stop's own message waits for it, and the end is silent.
#[tokio::test]
#[serial]
async fn stopcomm_while_a_tester_present_is_outstanding_waits_and_stays_silent() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cfg = Cfg {
        p2_max_us: 700_000,
        ..Cfg::default()
    };
    let (cll, mut events) = setup_link(&mut client, cfg, 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let sent_at = std::time::Instant::now();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![0x10, 0x01],
            cop_ctrl_data: None,
        })
        .await
        .expect("CoptStopcomm should succeed");

    tokio::time::sleep(ms(250)).await;
    assert_eq!(
        written(&server).len(),
        1,
        "the stop's message must wait for the outstanding tester-present"
    );
    wait_until(
        "the stop's message after the window ended",
        ms(2000),
        || written(&server).len() == 2,
    )
    .await;
    assert!(sent_at.elapsed() >= ms(600));
    assert_eq!(written(&server)[1], frame_bytes(0x7E0, &[0x10, 0x01]));
    assert!(
        !wait_for_event(&mut events, 300, is_tester_present_error).await,
        "after CoptStopcomm the end of the window is silent"
    );

    drop(events);
    server.shutdown().await;
}

/// A `CoptUpdateparam` that changes the tester-present's wire content re-arms
/// with a send that waits for the outstanding answer.
#[tokio::test]
#[serial]
async fn a_rearm_send_waits_for_the_outstanding_tester_present_answer() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let (cll, mut events) = setup_link(&mut client, Cfg::default(), 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    set_com_param_bytes(
        &mut client,
        cll,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E, 0x80],
    )
    .await;
    promote_via_update_param(&mut client, cll).await;

    tokio::time::sleep(ms(300)).await;
    assert_eq!(written(&server).len(), 1, "the re-arm's send must wait");
    inject(&server, 0x7E8, &[0x7E, 0x00]);
    wait_until("the re-arm's send after the answer", ms(1500), || {
        written(&server).len() == 2
    })
    .await;
    assert_eq!(written(&server)[1], frame_bytes(0x7E0, &[0x3E, 0x80]));

    drop(events);
    server.shutdown().await;
}

/// The expected answers are frozen at the send: promoting another
/// `CP_TesterPresentExpPosResp` while one is outstanding does not change how
/// that send settles.
#[tokio::test]
#[serial]
async fn the_expected_answer_is_frozen_at_the_send() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let (cll, mut events) = setup_link(&mut client, Cfg::default(), 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    set_com_param_bytes(&mut client, cll, CP_TESTER_PRESENT_EXP_POS_RESP, vec![0x5E]).await;
    promote_via_update_param(&mut client, cll).await;
    client_request(&mut client, cll, 1).await;

    inject(&server, 0x7E8, &[0x7E, 0x00]);
    wait_until("the request after the frozen answer", ms(1500), || {
        count_of(&server, &request_frame()) == 1
    })
    .await;
    assert!(!wait_for_event(&mut events, 200, is_tester_present_error).await);

    drop(events);
    server.shutdown().await;
}

/// A client request that waits for the tester-present can be cancelled; it is
/// never written.
#[tokio::test]
#[serial]
async fn a_client_request_waiting_for_the_tester_present_can_be_cancelled() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let (cll, mut events) = setup_link(&mut client, Cfg::default(), 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let cop = client_request(&mut client, cll, 1).await;
    tokio::time::sleep(ms(200)).await;
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    assert!(
        wait_for_event(&mut events, 1500, is_cancelled).await,
        "the waiting request should end as cancelled"
    );
    assert_eq!(count_of(&server, &request_frame()), 0);

    drop(events);
    server.shutdown().await;
}

/// A client request without a response (`NumReceiveCycles` 0) holds nothing:
/// the tester-present keeps its cadence around it.
#[tokio::test]
#[serial]
async fn a_client_send_without_response_does_not_hold_the_tester_present() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cfg = Cfg {
        time_us: 400_000,
        p2_max_us: 300_000,
        ..Cfg::default()
    };
    let (cll, mut events) = setup_link(&mut client, cfg, 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    inject(&server, 0x7E8, &[0x7E, 0x00]);
    send_data(&mut client, cll, vec![0x22, 0xF1, 0x90], vec![]).await;
    wait_until(
        "the second tester-present at its interval",
        ms(1500),
        || count_of(&server, &tp_frame()) >= 2,
    )
    .await;
    assert_eq!(count_of(&server, &request_frame()), 1);

    drop(events);
    server.shutdown().await;
}

/// An IS-CYCLIC request holds a physical tester-present until its first
/// response (Table 1's note); after it the request is receive only and holds
/// nothing.
#[tokio::test]
#[serial]
async fn an_is_cyclic_request_holds_the_tester_present_only_until_its_first_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cfg = Cfg {
        time_us: 400_000,
        p2_max_us: 300_000,
        ..Cfg::default()
    };
    let (cll, mut events) = setup_link(&mut client, cfg, 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    inject(&server, 0x7E8, &[0x7E, 0x00]);
    let cop = client_request(&mut client, cll, -1).await;
    wait_until("the request to be written", ms(2000), || {
        count_of(&server, &request_frame()) == 1
    })
    .await;
    tokio::time::sleep(ms(900)).await;
    assert_eq!(
        count_of(&server, &tp_frame()),
        1,
        "no physical tester-present before the IS-CYCLIC request's first response"
    );

    inject(&server, 0x7E8, &[0x62, 0xF1, 0x90, 0x01]);
    wait_until(
        "the tester-present after the first response",
        ms(1500),
        || count_of(&server, &tp_frame()) >= 2,
    )
    .await;

    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop),
        })
        .await
        .expect("cancel_com_primitive should succeed");
    drop(events);
    server.shutdown().await;
}

/// A functional tester-present that expects no response goes at once while a
/// physical client request waits for its response.
#[tokio::test]
#[serial]
async fn a_functional_tester_present_without_response_goes_during_a_physical_wait() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cfg = Cfg {
        time_us: 300_000,
        req_rsp: 0,
        functional: true,
        ..Cfg::default()
    };
    let (cll, mut events) = setup_link(&mut client, cfg, 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    client_request(&mut client, cll, 1).await;
    wait_until("the request to be written", ms(2000), || {
        count_of(&server, &request_frame()) == 1
    })
    .await;
    let functional = frame_bytes(0x7DF, &[0x3E, 0x00]);
    tokio::time::sleep(ms(1000)).await;
    assert!(
        count_of(&server, &functional) >= 3,
        "functional tester-presents without response must keep going during the physical wait \
         (saw {})",
        count_of(&server, &functional)
    );

    inject(&server, 0x7E8, &[0x62, 0xF1, 0x90, 0x01]);
    assert!(wait_for_event(&mut events, 2000, is_finished).await);

    drop(events);
    server.shutdown().await;
}

/// A functional tester-present that expects a response holds the channel for
/// its whole window, answered or not (several ECUs may answer); an answer
/// only means the end is not an error.
#[tokio::test]
#[serial]
async fn a_functional_tester_present_with_response_holds_until_its_window_ends() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cfg = Cfg {
        p2_max_us: 700_000,
        functional: true,
        ..Cfg::default()
    };
    let (cll, mut events) = setup_link(&mut client, cfg, 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let sent_at = std::time::Instant::now();
    inject(&server, 0x7E8, &[0x7E, 0x00]);
    client_request(&mut client, cll, 1).await;

    tokio::time::sleep(ms(300)).await;
    assert_eq!(
        count_of(&server, &request_frame()),
        0,
        "an answer does not end a functional send's hold"
    );
    wait_until("the request after the window ended", ms(2000), || {
        count_of(&server, &request_frame()) == 1
    })
    .await;
    assert!(sent_at.elapsed() >= ms(600));
    assert!(
        !wait_for_event(&mut events, 200, is_tester_present_error).await,
        "a functional send that was answered ends without an error event"
    );

    drop(events);
    server.shutdown().await;
}

/// Mode 1 (idle-triggered): a tester-present held by a client wait re-idles
/// from the bus activity of the response instead of going at once.
#[tokio::test]
#[serial]
async fn mode_1_tester_present_reidles_after_the_held_exchange() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cfg = Cfg {
        time_us: 600_000,
        send_type: 1,
        ..Cfg::default()
    };
    let (cll, mut events) = setup_link(&mut client, cfg, 0x7E0, 0x7E8).await;

    start_comm(&mut client, cll, &mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    inject(&server, 0x7E8, &[0x7E, 0x00]);
    client_request(&mut client, cll, 1).await;
    tokio::time::sleep(ms(900)).await;
    inject(&server, 0x7E8, &[0x62, 0xF1, 0x90, 0x01]);
    assert!(wait_for_event(&mut events, 2000, is_finished).await);
    let finished_at = std::time::Instant::now();

    tokio::time::sleep(ms(300)).await;
    assert_eq!(
        count_of(&server, &tp_frame()),
        1,
        "mode 1 waits for the bus to be idle for its interval again after the response"
    );
    wait_until("the idle-triggered tester-present", ms(2500), || {
        count_of(&server, &tp_frame()) == 2
    })
    .await;
    assert!(finished_at.elapsed() >= ms(500));

    drop(events);
    server.shutdown().await;
}

/// Two links on one shared channel: one hold for the channel, so the
/// sibling's request and its `CoptStartcomm` arm wait for the first link's
/// outstanding tester-present alike.
#[tokio::test]
#[serial]
async fn two_links_on_one_channel_share_one_hold() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let (cll_a, mut events_a) = setup_link(&mut client, Cfg::default(), 0x7E0, 0x7E8).await;
    let (cll_b, events_b) = setup_link(&mut client, Cfg::default(), 0x7E1, 0x7E9).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "the two links should share one physical channel"
    );

    start_comm(&mut client, cll_a, &mut events_a).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Link B: a request and then its arm, both behind A's tester-present.
    client_request(&mut client, cll_b, 1).await;
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("CoptStartcomm should succeed");
    tokio::time::sleep(ms(400)).await;
    assert_eq!(
        written(&server).len(),
        1,
        "the sibling link's frames must wait for the outstanding tester-present"
    );

    inject(&server, 0x7E8, &[0x7E, 0x00]);
    wait_until("the sibling's request after the answer", ms(1500), || {
        written(&server).len() >= 2
    })
    .await;
    assert_eq!(written(&server)[1], frame_bytes(0x7E1, &[0x22, 0xF1, 0x90]));

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// Disconnect releases the hold: the entries of the disconnected link are
/// cleared, so a sibling's request goes at once.
#[tokio::test]
#[serial]
async fn a_disconnect_releases_the_hold_of_the_disconnected_link() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let (cll_a, mut events_a) = setup_link(&mut client, Cfg::default(), 0x7E0, 0x7E8).await;
    let (cll_b, events_b) = setup_link(&mut client, Cfg::default(), 0x7E1, 0x7E9).await;
    assert_eq!(server.backdoor.connect_count(), 1);

    start_comm(&mut client, cll_a, &mut events_a).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    client_request(&mut client, cll_b, 1).await;
    tokio::time::sleep(ms(300)).await;
    assert_eq!(written(&server).len(), 1);

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");
    wait_until(
        "the sibling's request after the disconnect",
        ms(1500),
        || written(&server).len() >= 2,
    )
    .await;

    drop(events_a);
    drop(events_b);
    server.shutdown().await;
}

/// ADR-418 item 3: a `CoptStartcomm` that carries a request completes that
/// request's response wait before the arm's first tester-present goes out, and
/// the answer to the session control is not mistaken for the tester-present's.
#[tokio::test]
#[serial]
async fn a_startcomm_request_completes_before_the_first_tester_present() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let (cll, mut events) = setup_link(&mut client, Cfg::default(), 0x7E0, 0x7E8).await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x10, 0x03],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 1,
                    mask_data: vec![0xFF],
                    pattern_data: vec![0x50],
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("CoptStartcomm with a request should succeed");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    assert_eq!(written(&server)[0], frame_bytes(0x7E0, &[0x10, 0x03]));
    tokio::time::sleep(ms(300)).await;
    assert_eq!(
        written(&server).len(),
        1,
        "the first tester-present follows the request's final response"
    );

    inject(&server, 0x7E8, &[0x50, 0x03, 0x00, 0x32]);
    assert!(wait_for_event(&mut events, 2000, is_finished).await);
    wait_until("the first tester-present", ms(1500), || {
        written(&server).len() == 2
    })
    .await;
    assert_eq!(written(&server)[1], tp_frame());

    drop(events);
    server.shutdown().await;
}

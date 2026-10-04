//! Response-pending / re-request NRC (0x21/0x23/0x78) auto-handling
//! (ADR-018). NRC 0x78 (ResponsePending) is the exception: `CP_P2Star`
//! reloads the response deadline to `now + CP_P2Star` on *every* occurrence
//! (ISO 14229-2 §7.3 P2*client semantics, ADR-102), and `CP_RC78CompletionTimeout`
//! is an independent, optional total-duration ceiling anchored once at the
//! first 0x78. NRC 0x21/0x23 have no analogous reload mechanism: each
//! response code's `CP_RC*CompletionTimeout` remains a fixed ceiling
//! anchored at that code's *first* occurrence in the COP, not a window that
//! a later repeat of the same code pushes further out (ADR-057).

use serial_test::serial;
use vci_service_interface::{
    ComPrimitiveCtrlData, ComPrimitiveHandle, EventNotification, ExpectedResponseData,
    PduErrorEvent, StartComPrimitiveRequest, SubscribeEventRequest, event_item,
    subscribe_event_request, vci_service_client::VciServiceClient,
};

use crate::harness::*;

const CP_RC_BYTE_OFFSET: u32 = 0x8028;
const CP_RC78_HANDLING: u32 = 0x8027;
const CP_P2_STAR: u32 = 0x8011;
const CP_RC78_COMPLETION_TIMEOUT: u32 = 0x8026;
/// RC21 (NRC 0x21, `BusyRepeatRequest`) auto re-request ComParam ID
/// (ADR-018), same value `stopcomm_data_tx.rs`/`concat.rs`/`cop_ctrl_cycles.rs`
/// pin for their own RC21 coverage.
const CP_RC21_HANDLING: u32 = 0x8021;

async fn start_send_recv(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
    cop_data: Vec<u8>,
) -> ComPrimitiveHandle {
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
        .expect("start_com_primitive should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
}

async fn subscribe(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
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

/// A UDS negative response (`0x7F <SID> <NRC>`) for request SID `0x22`, on
/// CAN ID `0x7E8`. RC-byte-offset detection runs against the payload with
/// the CAN ID header already split off (ADR-051), so the NRC is at
/// `payload[2]` (`CP_RCByteOffset = 2`) -- not `data[6]` as ADR-018's
/// pre-ADR-051 worked example still (incorrectly) says.
fn negative_response(nrc: u8) -> Vec<u8> {
    can_frame(0x7E8, &[0x7F, 0x22, nrc])
}

/// ADR-102: `CP_P2Star` is a per-occurrence reload window, not a ceiling --
/// each 0x78 must reload the response deadline to `now + CP_P2Star`
/// (ISO 14229-2 §7.3 P2*client semantics), so a *second* 0x78 sent partway
/// through the first window must push the deadline further out. This
/// supersedes ADR-057's anchor-once treatment of this same value, which is
/// now exclusive to `CP_RC78CompletionTimeout` (see the ceiling test below).
#[tokio::test]
#[serial]
async fn p2_star_reloads_deadline_on_every_078_occurrence() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            // Deliberately much larger than CP_P2Star below, so the base
            // response window can never be what ends the COP -- only the
            // reloaded CP_P2Star deadline can (isolating exactly what this
            // test checks).
            (j2534_0404::P2_MAX, 10_000_000),
            (CP_RC78_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_P2_STAR, 300_000), // 300 ms reload window, stored in us
                                   // CP_RC78CompletionTimeout deliberately left unset: no ceiling.
        ],
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
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop = start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    let is_finished_for_cop = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop.cop_handle)
    };
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // First 0x78 at t=0: under the old anchor-once behavior this would fix
    // the deadline at t=300 ms.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &negative_response(0x78),
        j2534_0404::ISO15765,
    );

    // Second 0x78 at t=150 ms, well within that window: with reload
    // semantics the deadline moves out to t=450 ms.
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &negative_response(0x78),
        j2534_0404::ISO15765,
    );

    // Up to ~t=350 ms, the COP must NOT have finished yet -- under the old
    // anchor-once bug it would already have finished at t=300 ms,
    // comfortably inside this window.
    assert!(
        !wait_for_event(&mut events, 200, is_finished_for_cop).await,
        "COP should not finish at the first occurrence's t=300ms deadline; CP_P2Star must \
         reload on the second 0x78, not stay anchored to the first"
    );

    // From here (~t=350ms), the reloaded deadline (t=450ms) is close;
    // give a generous bound.
    assert!(
        wait_for_event(&mut events, 300, is_finished_for_cop).await,
        "COP should finish ~300ms after the second 0x78 (t=450ms), proving CP_P2Star reloaded \
         rather than staying anchored to the first occurrence"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-102: `CP_RC78CompletionTimeout` is reinstated as an independent
/// total-duration ceiling, anchored once at the *first* 0x78, that bounds
/// the whole (reloading) 0x78 sequence regardless of how many further
/// occurrences arrive. With `CP_P2Star` set to a 300 ms reload window and
/// `CP_RC78CompletionTimeout` set to a 400 ms ceiling, repeated 0x78's
/// every ~150 ms -- close enough together to keep reloading CP_P2Star's own
/// window past what a single 300 ms window would allow -- must still see
/// the COP finish at the 400 ms ceiling, not later.
#[tokio::test]
#[serial]
async fn rc78_completion_timeout_bounds_the_reloaded_078_deadline_as_ceiling() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            // Deliberately much larger than the ceiling below, so the base
            // response window can never be what ends the COP.
            (j2534_0404::P2_MAX, 10_000_000),
            (CP_RC78_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 2),
            (CP_P2_STAR, 300_000),                 // 300 ms reload window
            (CP_RC78_COMPLETION_TIMEOUT, 400_000), // 400 ms total ceiling
        ],
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
    let mut events = subscribe(&mut client, cll_handle).await;

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // 0x78's at t=0, 150, 300 ms: each reloads CP_P2Star's own 300 ms
    // window (which alone would push the deadline out to t=600ms), but the
    // 400 ms ceiling anchored at the first occurrence must still cut the
    // COP off at t=400 ms.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &negative_response(0x78),
        j2534_0404::ISO15765,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &negative_response(0x78),
        j2534_0404::ISO15765,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &negative_response(0x78),
        j2534_0404::ISO15765,
    );

    // From t=300 ms, the 400 ms ceiling is only ~100 ms away, but an
    // unbounded reload (the bug this ceiling exists to prevent) would keep
    // pushing the deadline out to t=600 ms and beyond -- a 250 ms bound
    // comfortably distinguishes the two.
    assert!(
        wait_for_event(&mut events, 250, is_finished).await,
        "COP should finish at the CP_RC78CompletionTimeout ceiling (t=400ms), not be pushed out \
         by CP_P2Star's own per-occurrence reload"
    );

    drop(events);
    server.shutdown().await;
}

/// Closes the backlog
/// entry citing ADR-087: proves the RC21 (NRC 0x21, `BusyRepeatRequest`)
/// auto re-request path's own retransmit-failure handling
/// (`TxFailure::Event(PduErrEvtTxError)` -> `wait_for_expected_response_inner`
/// -> `ReceivePhaseOutcome::ReRequestTxFailed`) is reachable and handled
/// correctly for `CoptSendrecv`'s own caller (`handle_send_recv`): per
/// ADR-087, that caller emits `PduCopstFinished` itself on this outcome
/// (byte-for-byte its pre-ADR-087 behavior), after the TX-error event
/// already emitted inside the wait. This is genuinely discriminating -- if
/// `ReRequestTxFailed`'s handling were removed/broken, the COP would either
/// hang (never reaching `PduCopstFinished`) or never surface the
/// `PduErrEvtTxError` event at all.
///
/// The write-failure override (`__mock_set_write_msgs_error`) is armed
/// *before* the RC21 is injected -- the original request's own first write
/// (before arming) must succeed, since the ECU needs something to
/// NRC-reject, but once armed the override applies to every subsequent
/// `PassThruWriteMsgs` call regardless of timing, so this test has no race
/// against `CP_RC21RequestTime`'s (here, unset/zero, i.e. no sleep)
/// retry-sleep scheduling.
#[tokio::test]
#[serial]
async fn rc21_retransmit_write_failure_still_finishes_the_cop_sendrecv() {
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
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
            ],
        )],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop = start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90]).await;
    let is_finished_for_cop = |item: &vci_service_interface::EventItem| {
        is_finished(item)
            && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop.cop_handle)
    };

    // The original request's own first write must succeed -- confirm it
    // before arming the override below.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Arm the write-failure override before the RC21 is ever injected: the
    // retransmit it triggers is then guaranteed to fail, with no timing
    // race against CP_RC21RequestTime's retry-sleep scheduling.
    server
        .backdoor
        .set_write_msgs_error(Some(j2534_0404::ERR_FAILED as std::os::raw::c_long));

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &negative_response(0x21),
        j2534_0404::ISO15765,
    );

    let mut saw_tx_error = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == PduErrorEvent::PduErrEvtTxError as i32
            ) {
                saw_tx_error = true;
            }
            is_finished_for_cop(item)
        })
        .await,
        "the RC21 retransmit-failure path should still finish the COP (PduCopstFinished), not \
         hang"
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

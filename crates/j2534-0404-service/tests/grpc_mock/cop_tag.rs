//! `StartComPrimitiveRequest.cop_tag`/`EventItem.cop_tag` (ADR-204): a
//! client-chosen, opaque correlation token, echoed verbatim on every
//! `EventItem` this crate emits for the resulting COP whenever `cop_handle`
//! is present -- `cop_status` transitions, `result_data` (a `CoptSendrecv`
//! response), and `error_data` (a COP-scoped error) alike -- see
//! `docs/rpc-api-guide.md`'s "Event correlation (`cop_tag`, ADR-204)"
//! section for the full contract.
//!
//! `CoptDelay` is used throughout (mirrors `cop_ctrl_cycles.rs`'s own
//! `queue_delay` helper): it needs no addressing ComParams and reliably
//! produces a `PDU_COPST_EXECUTING` event immediately at dispatch, then a
//! `PDU_COPST_FINISHED` event once the delay elapses -- exactly the
//! COP-status event type `cop_tag` is echoed on.

use serial_test::serial;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveCtrlData, EventNotification, ExpectedResponseData,
    PduComPrimitiveStatus, StartComPrimitiveRequest, SubscribeEventRequest, event_item,
    event_notification, subscribe_event_request, vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// `CP_CyclicRespTimeout` ComParam id (SAE J2534-2 clause 9), mirroring
/// `cop_ctrl_cycles.rs`'s own file-local constant of the same name/value --
/// not shared via `harness.rs`.
const CP_CYCLIC_RESP_TIMEOUT: u32 = 0x8010;

/// Same shape as every other test file's own `subscribe` helper (not shared
/// via `harness.rs`).
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

fn delay_request(
    cll_handle: ComLogicalLinkHandle,
    cop_tag: Option<Vec<u8>>,
) -> StartComPrimitiveRequest {
    StartComPrimitiveRequest {
        cop_tag,
        cll_handle: Some(cll_handle),
        cop_type: vci_service_interface::ComOperationType::CoptDelay as i32,
        cop_data: vec![],
        cop_ctrl_data: Some(ComPrimitiveCtrlData {
            time: 30,
            num_send_cycles: 0,
            num_receive_cycles: 0,
            temp_param_update: 0,
            expected_response_array: Vec::<ExpectedResponseData>::new(),
            tx_flag: None,
        }),
    }
}

fn is_finished(item: &vci_service_interface::EventItem) -> bool {
    matches!(
        item.data,
        Some(event_item::Data::CopStatus(status))
            if status == PduComPrimitiveStatus::PduCopstFinished as i32
    )
}

/// A `CoptSendrecv` request awaiting `expected_response_array` matches,
/// mirroring `cop_ctrl_cycles.rs`'s own `start_send_recv` helper but exposing
/// `cop_tag` (that helper hardcodes `cop_tag: None`).
fn send_recv_request(
    cll_handle: ComLogicalLinkHandle,
    cop_tag: Option<Vec<u8>>,
    cop_data: Vec<u8>,
    num_receive_cycles: i32,
    expected_response_array: Vec<ExpectedResponseData>,
) -> StartComPrimitiveRequest {
    StartComPrimitiveRequest {
        cop_tag,
        cll_handle: Some(cll_handle),
        cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
        cop_data,
        cop_ctrl_data: Some(ComPrimitiveCtrlData {
            time: 0,
            num_send_cycles: 1,
            num_receive_cycles,
            temp_param_update: 0,
            expected_response_array,
            tx_flag: None,
        }),
    }
}

/// An `ExpectedResponseData` accepting any positive ReadDataByIdentifier
/// response (first payload byte 0x62) with the given `acceptance_id` --
/// mirrors `cop_ctrl_cycles.rs`'s own `expect_positive_response` helper (not
/// shared via `harness.rs`).
fn expect_positive_response(acceptance_id: u32) -> ExpectedResponseData {
    ExpectedResponseData {
        response_type: 0,
        acceptance_id,
        mask_data: vec![0xFF],
        pattern_data: vec![0x62],
        unique_resp_ids: vec![],
    }
}

/// A tag supplied at `StartComPrimitive` is echoed verbatim on the COP's
/// `PDU_COPST_FINISHED` event, delivered live via `SubscribeEvent`.
#[tokio::test]
#[serial]
async fn cop_tag_round_trips_to_the_finished_event() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let tag = b"client-correlation-token".to_vec();
    client
        .start_com_primitive(delay_request(cll_handle, Some(tag.clone())))
        .await
        .expect("start_com_primitive(CoptDelay) should succeed");

    let mut observed_tag = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if is_finished(item) {
                observed_tag = Some(item.cop_tag.clone());
                true
            } else {
                false
            }
        })
        .await,
        "CoptDelay should finish"
    );
    assert_eq!(
        observed_tag,
        Some(Some(tag)),
        "the FINISHED event should echo the tag supplied at StartComPrimitive"
    );

    drop(events);
    server.shutdown().await;
}

/// No `cop_tag` supplied at `StartComPrimitive` -> `EventItem.cop_tag` is
/// absent on every event for that COP, never a default/empty value.
#[tokio::test]
#[serial]
async fn no_cop_tag_leaves_event_item_cop_tag_absent() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    client
        .start_com_primitive(delay_request(cll_handle, None))
        .await
        .expect("start_com_primitive(CoptDelay) should succeed");

    let mut observed_tag = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if is_finished(item) {
                observed_tag = Some(item.cop_tag.clone());
                true
            } else {
                false
            }
        })
        .await,
        "CoptDelay should finish"
    );
    assert_eq!(
        observed_tag,
        Some(None),
        "no cop_tag was supplied, so EventItem.cop_tag must be absent"
    );

    drop(events);
    server.shutdown().await;
}

/// `StartComPrimitiveRequest.cop_tag` past the documented maximum size
/// (`MAX_COP_TAG_LEN` = 64 bytes, `rpc_primitive.rs`) is rejected
/// synchronously, before the COP is ever created -- an unbounded tag would
/// be a per-event amplification hazard, since it is echoed on every
/// `cop_status` event the COP produces over its whole lifetime.
#[tokio::test]
#[serial]
async fn oversized_cop_tag_is_rejected_before_the_cop_is_created() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let oversized_tag = vec![0u8; 65];
    let status = client
        .start_com_primitive(delay_request(cll_handle, Some(oversized_tag)))
        .await
        .expect_err("an oversized cop_tag should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("cop_tag"), "{}", status.message());

    // A tag right at the documented limit is accepted (boundary check).
    let max_tag = vec![0u8; 64];
    client
        .start_com_primitive(delay_request(cll_handle, Some(max_tag)))
        .await
        .expect("a cop_tag exactly at MAX_COP_TAG_LEN should be accepted");

    server.shutdown().await;
}

/// Race-shaped regression test (ADR-204's Consequences section): this
/// crate's poll-task architecture (ADR-021) dispatches an enqueued COP
/// independently of `rpc_start_com_primitive`'s own return to the caller, so
/// a live `SubscribeEvent` listener can observe this COP's first event
/// before the client has even finished awaiting `StartComPrimitiveResponse`.
/// `cop_tag` must still be attributable from the event alone, with no
/// dependency on having consumed the Start response first.
#[tokio::test]
#[serial]
async fn cop_tag_is_attributable_from_an_event_observed_before_the_start_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let tag = b"race-token".to_vec();
    let mut start_client = server.client().await;
    let start_request = delay_request(cll_handle, Some(tag.clone()));
    // Spawned, not awaited here: the whole point is to read a live event off
    // `events` before this task's own response is ever consumed.
    let start_task =
        tokio::spawn(async move { start_client.start_com_primitive(start_request).await });

    // Read live notifications for this CLL -- skipping any pre-existing
    // backlog `SubscribeEvent` flushes on attach (e.g. the `PDU_CLLST_ONLINE`
    // queued by the earlier `ConnectComLogicalLink`) -- until this COP's own
    // `PDU_COPST_EXECUTING` arrives. Crucially, `start_task`'s own result is
    // never touched anywhere in this loop: the whole point is to attribute
    // the event to `tag` before the Start response has been consumed at all.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(2000);
    let item = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for PDU_COPST_EXECUTING"
        );
        let notification = tokio::time::timeout(remaining, events.message())
            .await
            .expect("should not time out waiting for the next live event")
            .expect("stream should not error")
            .expect("stream should not end");
        let item = match notification.event_data {
            Some(event_notification::EventData::Item(item)) => item,
            other => panic!("expected an EventItem notification, got {other:?}"),
        };
        if matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstExecuting as i32
        ) {
            break item;
        }
    };
    assert_eq!(
        item.cop_tag,
        Some(tag),
        "the tag must be readable off the event before the Start response is ever consumed"
    );

    start_task
        .await
        .expect("start task should not panic")
        .expect("start_com_primitive(CoptDelay) should succeed");

    drop(events);
    server.shutdown().await;
}

/// A `CoptSendrecv` COP's `ResultData` event -- the actual response payload,
/// and the primary use case ADR-204's own motivating scenario cites -- echoes
/// the tag supplied at `StartComPrimitive`. This is the case that was
/// silently broken by the initial ADR-204 implementation: `cop_tag` was
/// threaded through `send_cop_status`/`StatusEvent::Cop` only, leaving the
/// `Frame`/`ResultData` arm of `cll_queue_item_to_event_item`
/// (`events_event_senders.rs`) hardcoded to `cop_tag: None`.
#[tokio::test]
#[serial]
async fn cop_tag_round_trips_to_a_result_data_event() {
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

    let tag = b"result-data-token".to_vec();
    client
        .start_com_primitive(send_recv_request(
            cll_handle,
            Some(tag.clone()),
            vec![0x22, 0xF1, 0x90],
            1,
            vec![expect_positive_response(9)],
        ))
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &[0x62, 0xF1, 0x90, 0x01]),
        j2534_0404::ISO15765,
    );

    let mut observed_tag = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ResultData(result)) = &item.data
                && result.acceptance_id == 9
            {
                observed_tag = Some(item.cop_tag.clone());
                true
            } else {
                false
            }
        })
        .await,
        "the matching response should be delivered as ResultData"
    );
    assert_eq!(
        observed_tag,
        Some(Some(tag)),
        "the ResultData event must echo the tag supplied at StartComPrimitive"
    );

    drop(events);
    server.shutdown().await;
}

/// A COP-scoped error event (`PDU_ERR_EVT_RX_TIMEOUT`, raised when a finite-N
/// receive-only registrant's `CP_CyclicRespTimeout` elapses with its target
/// count unmet, ADR-182) also echoes the tag supplied at `StartComPrimitive`
/// -- the `Error`/`ErrorData` arm of `cll_queue_item_to_event_item` had the
/// same `cop_tag: None` hardcoding bug as the `Frame`/`ResultData` arm
/// (`cop_tag_round_trips_to_a_result_data_event`'s own doc comment).
#[tokio::test]
#[serial]
async fn cop_tag_round_trips_to_a_cop_scoped_error_event() {
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
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let tag = b"error-token".to_vec();
    let response = client
        .start_com_primitive(send_recv_request(
            cll_handle,
            Some(tag.clone()),
            vec![0x22, 0xF1, 0x90],
            2,
            vec![expect_positive_response(9)],
        ))
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed")
        .into_inner();
    let cop_handle = response.cop_handle.expect("cop_handle should be present");

    // No response is ever injected, so CP_CyclicRespTimeout elapses with the
    // target count (2) unmet, raising PDU_ERR_EVT_RX_TIMEOUT for this COP.
    let mut observed_tag = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32
            ) && item
                .cop_handle
                .as_ref()
                .is_some_and(|h| h.cop_handle == cop_handle.cop_handle)
            {
                observed_tag = Some(item.cop_tag.clone());
                true
            } else {
                false
            }
        })
        .await,
        "CP_CyclicRespTimeout should raise PDU_ERR_EVT_RX_TIMEOUT for this COP"
    );
    assert_eq!(
        observed_tag,
        Some(Some(tag)),
        "the COP-scoped error event must echo the tag supplied at StartComPrimitive"
    );

    drop(events);
    server.shutdown().await;
}

/// Codex review finding on PR #116 (ADR-204 follow-up): `reap_expired_
/// cyclic_registrants` (`events.rs`) used to resolve a reaped COP's
/// `cop_tag` fresh, via `send_error_event`'s own internal `primitives`
/// lookup, AFTER the critical section that decided the COP was being reaped
/// had already released `logical_links`/`primitives` -- a window in which a
/// concurrent finalization path (`CancelComPrimitive`, `cancel_link_cops`, a
/// hard-error path, or even `reap_cancelled_detached_registrants` earlier in
/// the same tick) could remove the SAME `cop_handle`'s `primitives` entry
/// first, silently downgrading its `PDU_ERR_EVT_RX_TIMEOUT` event's
/// `cop_tag` to absent. Fixed by capturing each reaped registrant's
/// `cop_tag` inside that same critical section (`ReapedReceiveOnly::
/// cop_tag`) and threading it through `send_error_event_with_tag` instead of
/// re-resolving it later.
///
/// This test runs TWO finite-N cyclic COPs with distinct tags concurrently,
/// both reaped via `CP_CyclicRespTimeout` expiry, to guard against a
/// mismatched-keying regression in the per-`cop_handle` `primitives` lookup
/// this fix added (e.g. accidentally attributing one COP's tag to the
/// other's event) -- not just that a lone COP's tag survives, which
/// `cop_tag_round_trips_to_a_cop_scoped_error_event` above already covers.
///
/// **Scope note, corrected (edge-case-hunter verification, 2026-08-31): this
/// test does NOT discriminate the fix it was originally written to cover.**
/// Neither this test's two COPs ever have their `primitives` entry removed
/// between collection and emission -- both stay present in `primitives` the
/// whole time regardless of which code path resolves the tag -- so even the
/// OLD, reverted fresh-lookup code (`send_error_event`) passes this test
/// unchanged (empirically confirmed by reverting the fix's one-line wiring
/// change and re-running this test: it still passes). What this test DOES
/// prove is a real, distinct property: no cross-COP tag mixup when two COPs
/// are reaped in the same tick's sweep. The actual capture-survives-removal
/// property this fix depends on is covered by `events_reap_expired_cyclic_
/// registrants_cop_tag_tests::cop_tag_capture_survives_a_primitives_removal_
/// racing_the_later_emission` (`j2534-0404-service/src/service/events_reap_
/// expired_cyclic_registrants_cop_tag_tests.rs`), which drives `reap_
/// expired_cyclic_registrants` directly and forces a concurrent `primitives`
/// removal deterministically via `tokio::sync::Mutex`'s FIFO fairness (no
/// real concurrency or timing margin needed, unlike the case below) --
/// confirmed to fail against the reverted wiring and pass against the fix.
///
/// This test's own citation of `receive_only_finite_n_cancel_races_cyclic_
/// timeout_expiry_stays_cancelled` (`cop_ctrl_cycles.rs`) as precedent for
/// "burst experimentation proves an untestable same-tick race" previously
/// mischaracterized that test's own doc comment, which says the opposite:
/// its burst-of-40-concurrent-cancels technique showed NO measurable
/// difference between the fix present and reverted (it never reproduced the
/// S5-vs-S6 race it targets) -- the REAL regression coverage for that fix is
/// `events_reap_expired_cyclic_decision_tests.rs`'s deterministic unit tests
/// of the extracted, pure `reap_expired_cyclic_decision` function. That
/// precedent remains valid for its OWN race (a gap between two SEPARATE
/// ticks, with no reusable contention point to force it), but was never
/// good precedent for treating burst experimentation as sufficient
/// verification -- it explicitly is not, in that same file's own account.
#[tokio::test]
#[serial]
async fn cop_tag_is_attributed_correctly_when_multiple_cyclic_cops_are_reaped_concurrently() {
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
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let tag_a = b"cop-a-token".to_vec();
    let tag_b = b"cop-b-token".to_vec();

    let cop_a = client
        .start_com_primitive(send_recv_request(
            cll_handle,
            Some(tag_a.clone()),
            vec![0x22, 0xF1, 0x90],
            2,
            vec![expect_positive_response(9)],
        ))
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed for cop_a")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    let cop_b = client
        .start_com_primitive(send_recv_request(
            cll_handle,
            Some(tag_b.clone()),
            vec![0x22, 0xF1, 0x91],
            2,
            vec![expect_positive_response(10)],
        ))
        .await
        .expect("start_com_primitive(CoptSendrecv) should succeed for cop_b")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    // Neither response is ever injected, so both COPs' CP_CyclicRespTimeout
    // deadlines expire, unmatched -- both are live in `primitives`
    // simultaneously (started a few ms apart against a 200ms deadline), so a
    // mismatched-keying bug in the per-cop_handle tag lookup would show up as
    // a swapped or missing tag on one of the two events below.
    let mut observed_a = None;
    let mut observed_b = None;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if let Some(event_item::Data::ErrorData(error)) = &item.data
                && *error == vci_service_interface::PduErrorEvent::PduErrEvtRxTimeout as i32
            {
                if item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop_a.cop_handle)
                {
                    observed_a = Some(item.cop_tag.clone());
                } else if item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop_b.cop_handle)
                {
                    observed_b = Some(item.cop_tag.clone());
                }
            }
            observed_a.is_some() && observed_b.is_some()
        })
        .await,
        "both finite-N cyclic COPs should raise PDU_ERR_EVT_RX_TIMEOUT"
    );
    assert_eq!(
        observed_a,
        Some(Some(tag_a)),
        "cop_a's own error event must carry cop_a's own tag, not cop_b's or none"
    );
    assert_eq!(
        observed_b,
        Some(Some(tag_b)),
        "cop_b's own error event must carry cop_b's own tag, not cop_a's or none"
    );

    drop(events);
    server.shutdown().await;
}

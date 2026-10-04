//! `CP_EnableConcatenation` (D-PDU `PARAM_ENABLE_CONCATENATION` = 0x807B,
//! ISO 22900-2:2022 Table B.11): on the KWP family (ISO 9141-2, ISO
//! 14230-2/-4) and SAE J1850 VPW/PWM, when enabled, multiple physical
//! response messages sharing the same `unique_resp_identifier`/SID are
//! merged into ONE logical `ResultData` delivery instead of being delivered
//! separately (see ADR-148 for the full design).
//!
//! These tests exercise the feature through the full `CoptSendrecv`/
//! `CoptStopcomm` + `PassThruReadMsgs` poll-task stack (unlike
//! `events::bind_frame_tests`'s unit-level coverage of
//! `bind_registrant`/`bind_frame` directly), on ISO14230 with no
//! `UniqueRespIdTable` configured (so every response routes with
//! `unique_resp_identifier == 0`, mirroring
//! `rx_header_split::iso14230_protocol_parses_variable_length_kwp_header_on_rx`'s
//! own "no table" premise).

use serial_test::serial;
use vci_service_interface::{
    ComLogicalLinkHandle, ComOperationType, ComPrimitiveCtrlData, ComPrimitiveHandle,
    EventNotification, ExpectedResponseData, PduComPrimitiveStatus, StartComPrimitiveRequest,
    SubscribeEventRequest, event_item, event_notification, subscribe_event_request,
    vci_service_client::VciServiceClient,
};

use crate::harness::*;

fn is_finished(item: &vci_service_interface::EventItem) -> bool {
    matches!(
        item.data,
        Some(event_item::Data::CopStatus(status))
            if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
    )
}

/// D-PDU `CP_EnableConcatenation` ComParam ID (`PDU_PC_COM` class, Unum32,
/// ISO 22900-2:2022 Table B.11: KWP family / SAE J1850 VPW/PWM only). Not
/// exported by the crate, so duplicated here as a literal for the test, same
/// convention as this file's sibling `CP_*` constants in `harness.rs`.
const CP_ENABLE_CONCATENATION: u32 = 0x807B;

/// RC21 (NRC 0x21, `BusyRepeatRequest`) auto re-request ComParam IDs
/// (ADR-018), same values `stopcomm_data_tx.rs` pins for its own
/// `stopcomm_is_multiple_rc21_request_time_bounded_by_match_reset_ceiling`
/// test -- reused here (edge-case-hunter review of PR #18 round 14) to
/// combine that test's CEILING/RC21 setup with this file's own CONCAT setup.
const CP_RC21_HANDLING: u32 = 0x8021;
const CP_RC21_REQUEST_TIME: u32 = 0x8022;
const CP_RC_BYTE_OFFSET: u32 = 0x8028;

/// A 4-byte KWP2000 header: format `0x80` (addressed, separate length byte),
/// `target`, `source`, then the length byte -- the same on-wire shape
/// `rx_header_split::iso14230_protocol_parses_variable_length_kwp_header_on_rx`
/// uses. `target`/`source` are irrelevant to routing here (no table is
/// configured, so every response delivers with `unique_resp_identifier ==
/// 0` regardless), kept fixed for readability.
fn kwp_frame(payload: &[u8]) -> Vec<u8> {
    kwp_frame_from(0xF1, payload)
}

/// Same shape as `kwp_frame`, but with the header's source-address byte
/// (`ADR-148` third Amendment's `source_id` key component) set explicitly --
/// used to simulate two distinct ECUs answering the same functional/
/// broadcast request under the same (table-less) `unique_resp_identifier`.
fn kwp_frame_from(source: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x80, 0x10, source, payload.len() as u8];
    frame.extend_from_slice(payload);
    frame
}

/// A single-`CoptSendrecv` KWP request with one vacuous (empty mask/pattern
/// -- matches any payload) expected-response descriptor, `num_send_cycles:
/// 1`, and the given `num_receive_cycles`.
async fn start_vacuous_send_recv(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
    num_receive_cycles: i32,
) -> vci_service_interface::ComPrimitiveHandle {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 1,
                    mask_data: Vec::new(),
                    pattern_data: Vec::new(),
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

/// Starts a `CoptStartcomm` and returns its `cop_handle` -- `CoptStopcomm`
/// requires `comm_started == true` (`rpc_primitive.rs`'s pre-flight check),
/// same pattern `stopcomm_data_tx.rs`'s own `start_comm` helper uses.
async fn start_comm(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
) -> ComPrimitiveHandle {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present")
}

/// Like `is_finished`, but qualified to `cop_handle` -- mirrors
/// `stopcomm_data_tx.rs`'s own `wait_for_cop_finished_matching` helper, so an
/// earlier, unrelated COP's own backlogged `PduCopstFinished` cannot be
/// mistaken for this one's.
async fn wait_for_cop_finished_matching(
    events: &mut tonic::Streaming<EventNotification>,
    cop_handle: ComPrimitiveHandle,
) {
    assert!(
        wait_for_event(events, 2_000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ) && item
            .cop_handle
            .as_ref()
            .is_some_and(|h| h.cop_handle == cop_handle.cop_handle))
        .await,
        "expected a PduCopstFinished event for cop_handle {cop_handle:?}"
    );
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

/// Collects `ResultData` items off a live `SubscribeEvent` stream until
/// `count` have arrived, or panics after `timeout_ms`.
async fn collect_result_data(
    events: &mut tonic::Streaming<EventNotification>,
    count: usize,
    timeout_ms: u64,
) -> Vec<vci_service_interface::ResultData> {
    let mut results = Vec::new();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
    while results.len() < count {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {count} ResultData item(s); got {}",
            results.len()
        );
        let Ok(Ok(Some(notification))) = tokio::time::timeout(remaining, events.message()).await
        else {
            panic!("event stream ended or errored before {count} ResultData item(s) arrived");
        };
        if let Some(event_notification::EventData::Item(item)) = notification.event_data
            && let Some(event_item::Data::ResultData(result)) = item.data
        {
            results.push(result);
        }
    }
    results
}

/// (a) Two segments sharing the same SID merge into ONE delivered
/// `ResultData` when `CP_EnableConcatenation = 1`. Only one segment pair is
/// ever sent, so this COP's own buffer is finalized by the `CP_P2Max`
/// deadline-expiry path (no differing-key frame arrives to trigger the
/// mid-stream finalize) -- the default 50 ms KWP `CP_P2Max` keeps this
/// bounded.
#[tokio::test]
#[serial]
async fn concat_enabled_merges_two_same_sid_segments_into_one_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (CP_ENABLE_CONCATENATION, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // Only one logical match is needed -- concatenation must merge both
    // segments into it rather than requiring two.
    start_vacuous_send_recv(&mut client, cll_handle, 1).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let segment_1 = vec![0x62, 0xAA, 0xBB];
    let segment_2 = vec![0x62, 0xCC, 0xDD];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&segment_1),
        j2534_0404::ISO14230,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&segment_2),
        j2534_0404::ISO14230,
    );

    let results = collect_result_data(&mut events, 1, 2_000).await;
    assert_eq!(
        results.len(),
        1,
        "the two segments must merge into ONE delivery"
    );
    assert_eq!(
        results[0].data_bytes,
        vec![0x62, 0xAA, 0xBB, 0xCC, 0xDD],
        "the merged payload is segment 1 in full, then segment 2 with its own leading SID byte \
         dropped"
    );

    drop(events);
    server.shutdown().await;
}

/// (b) Regression guard: with `CP_EnableConcatenation` left at its default
/// (0), the identical two-segment scenario still delivers TWO separate
/// `ResultData` items, unmerged -- backward compatibility with every
/// pre-existing (non-concat) KWP/J1850 `CoptSendrecv` flow.
#[tokio::test]
#[serial]
async fn concat_disabled_default_delivers_two_separate_responses() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
        // CP_EnableConcatenation deliberately left unset (default 0).
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // Two separate matches are required this time -- concatenation is off,
    // so each segment must complete its own match.
    start_vacuous_send_recv(&mut client, cll_handle, 2).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let segment_1 = vec![0x62, 0xAA, 0xBB];
    let segment_2 = vec![0x62, 0xCC, 0xDD];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&segment_1),
        j2534_0404::ISO14230,
    );
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&segment_2),
        j2534_0404::ISO14230,
    );

    let results = collect_result_data(&mut events, 2, 2_000).await;
    assert_eq!(
        results.len(),
        2,
        "each segment must still be delivered separately when concatenation is disabled"
    );
    assert_eq!(results[0].data_bytes, segment_1);
    assert_eq!(results[1].data_bytes, segment_2);

    drop(events);
    server.shutdown().await;
}

/// (c) A differing-SID frame arriving mid-accumulation opens its OWN fresh
/// buffer alongside the still-open original one (ADR-148 Amendment: a
/// differing key is never, by itself, a finalize trigger any more -- see the
/// ADR's Amendment section for the P1 bug this fixed under IS-MULTIPLE).
/// `CP_EnableConcatenation = 1`, `num_receive_cycles = 2`: with no third
/// frame ever arriving, BOTH buffers sit open until the shared `CP_P2Max`
/// receive-phase deadline expires, at which point they finalize TOGETHER in
/// one pass and are delivered as two separate `ResultData`s, in
/// first-opened-first (arrival) order -- SID 0x62 first, then SID 0x6A --
/// which happens to reproduce the same delivery values (though not the same
/// timing) the pre-amendment mechanism produced for this specific scenario.
#[tokio::test]
#[serial]
async fn concat_differing_sid_finalizes_open_buffer_and_delivers_it_separately() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (CP_ENABLE_CONCATENATION, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    start_vacuous_send_recv(&mut client, cll_handle, 2).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let sid_62 = vec![0x62, 0xAA, 0xBB];
    let sid_6a = vec![0x6A, 0xCC];
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &kwp_frame(&sid_62), j2534_0404::ISO14230);
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &kwp_frame(&sid_6a), j2534_0404::ISO14230);

    let results = collect_result_data(&mut events, 2, 2_000).await;
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].data_bytes, sid_62,
        "the SID-0x62 buffer, held open (not force-finalized) until the shared deadline, must \
         deliver its own (single-segment) contents unmerged, in first-opened-first order"
    );
    assert_eq!(
        results[1].data_bytes, sid_6a,
        "the differing-SID frame's own FRESH buffer, finalized in the same deadline-expiry pass \
         as the first, delivered separately"
    );

    drop(events);
    server.shutdown().await;
}

/// (d) ADR-148 third Amendment (Fix 1) regression at the full RPC/poll-task
/// level: two distinct ECUs (distinguished only by the header's own
/// source-address byte -- `kwp_frame_from`) answering the same functional/
/// broadcast request with the same SID, on this file's own no-`
/// UniqueRespIdTable` premise (so both share `unique_resp_identifier == 0`),
/// must merge into TWO separate, correctly-keyed responses -- not one
/// corrupted merge -- mirroring `cop_ctrl_cycles.rs`'s IS-MULTIPLE-style
/// `sendrecv_is_multiple_collects_all_matches_within_the_window` test's
/// two-ECU/`num_receive_cycles = -2` structure. Every other test in this file
/// uses `kwp_frame`'s fixed source address, so none of them would catch a
/// regression in `source_id` derivation/keying -- only
/// `events::bind_frame_tests`'s unit-level coverage did before this test.
#[tokio::test]
#[serial]
async fn concat_two_source_addresses_deliver_two_separate_merged_responses() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (CP_ENABLE_CONCATENATION, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // IS-MULTIPLE: no fixed match count, both ECUs' merged responses are
    // collected within the CP_P2Max window.
    start_vacuous_send_recv(&mut client, cll_handle, -2).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let ecu_a_1 = vec![0x62, 0xAA, 0xBB];
    let ecu_a_2 = vec![0x62, 0xCC, 0xDD];
    let ecu_b_1 = vec![0x62, 0x11, 0x22];
    let ecu_b_2 = vec![0x62, 0x33, 0x44];

    // Interleaved arrival (ECU A's first segment, ECU B's first segment,
    // then each ECU's second segment) so a `source_id`-keying regression
    // that collapsed both ECUs into one buffer would corrupt the merge
    // rather than happening to still produce correct output by luck.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame_from(0x10, &ecu_a_1),
        j2534_0404::ISO14230,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame_from(0x20, &ecu_b_1),
        j2534_0404::ISO14230,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame_from(0x10, &ecu_a_2),
        j2534_0404::ISO14230,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame_from(0x20, &ecu_b_2),
        j2534_0404::ISO14230,
    );

    let results = collect_result_data(&mut events, 2, 2_000).await;
    assert_eq!(
        results.len(),
        2,
        "both ECUs' merged responses must be delivered"
    );

    let merged_a = vec![0x62, 0xAA, 0xBB, 0xCC, 0xDD];
    let merged_b = vec![0x62, 0x11, 0x22, 0x33, 0x44];
    assert!(
        results.iter().any(|r| r.data_bytes == merged_a),
        "ECU A's own two segments must merge correctly, without ECU B's bytes mixed in"
    );
    assert!(
        results.iter().any(|r| r.data_bytes == merged_b),
        "ECU B's own two segments must merge correctly, without ECU A's bytes mixed in"
    );

    drop(events);
    server.shutdown().await;
}

/// Regression for a Codex P2 finding on ADR-148 Amendment 3: for an
/// IS-MULTIPLE (`NumReceiveCycles == -2`) COP with concatenation enabled,
/// `matches_needed` is `None`, so the deadline-expiry finalize path used to
/// never treat delivering its buffers as completion -- it always restarted
/// a SECOND full `CP_P2Max` window before the COP finished, even though the
/// very fact the deadline fired (triggering the finalize) already IS
/// IS-MULTIPLE's own "no new response this window" completion signal
/// (ADR-053). Bounds total elapsed time comfortably under two windows to
/// prove only one window elapses now, not two.
#[tokio::test]
#[serial]
async fn concat_is_multiple_completes_after_one_window_not_two() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Matches the window/units already established by
    // `cop_ctrl_cycles::sendrecv_is_multiple_collects_all_matches_within_the_window`
    // ("A short 400 ms window keeps the test fast").
    let window_raw: u32 = 400_000;
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::P2_MAX, window_raw),
            (CP_ENABLE_CONCATENATION, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop = start_vacuous_send_recv(&mut client, cll_handle, -2).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let start = tokio::time::Instant::now();
    // Two segments, same SID -- merge into one buffer, finalized only when
    // the deadline expires (no differing-key frame to force an earlier
    // mid-stream finalize).
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&[0x62, 0xAA]),
        j2534_0404::ISO14230,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&[0x62, 0xBB]),
        j2534_0404::ISO14230,
    );

    assert!(
        wait_for_event(&mut events, 3_000, |item| {
            is_finished(item)
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop.cop_handle)
        })
        .await,
        "the IS-MULTIPLE COP should finish once its buffer is finalized by the window closing"
    );
    let elapsed = start.elapsed();
    // Codex round-10 finding: the original 1_500ms bound was well above two
    // ~400ms windows (~800ms for the regressed `is_some_and` behavior), so
    // it passed for both the fixed and regressed implementations and never
    // actually protected this test's own named behavior. 700ms sits
    // comfortably between the expected one-window completion (~20ms to
    // absorb both segments + 400ms window + a small multiple of
    // `POLL_INTERVAL_MS` = 10ms to notice the deadline -- roughly 450ms)
    // and the regressed two-window completion (~850ms), leaving margin on
    // both sides for CI/harness jitter without admitting the bug.
    assert!(
        elapsed < tokio::time::Duration::from_millis(700),
        "completion took {elapsed:?}, expected well under two ~400ms windows -- a regression \
         back to `is_some_and` would needlessly await a second window before finishing"
    );

    drop(events);
    server.shutdown().await;
}

/// Coverage gap closed per edge-case-hunter review of PR #18 round 9: every
/// other test in this file that reaches the deadline-expiry finalize path's
/// `concat_segments_got = live_concat_segments_got` resync line (ADR-148
/// Amendment 8's second fix) has `matches_needed` exactly satisfied by that
/// same finalize -- either a finite count of 1, or IS-MULTIPLE (`None`,
/// which `is_none_or` always completes on any deadline-expiry finalize). The
/// wait loop returns `ReceivePhaseOutcome::CycleComplete` immediately after
/// the resync line runs, so its local `concat_segments_got` variable is
/// never read again in any existing test -- no existing test can tell the
/// resync line apart from it being missing or wrong entirely. This test
/// uses a FINITE `num_receive_cycles: 2` so the first deadline-expiry
/// finalize (match #1 of 2) does NOT complete the COP -- the loop must go
/// around for a SECOND `CP_P2Max` window, which is the only way the local
/// `concat_segments_got` baseline set by the resync line is ever read
/// again (as the baseline `poll_rx_and_check_match` diffs the second
/// window's own segments against).
///
/// `fail-without/pass-with proof` (this file's established convention, see
/// `concat_is_multiple_completes_after_one_window_not_two`): the resync
/// line was temporarily deleted and this test re-run (3x) to confirm a
/// regression is actually caught. It was NOT: elapsed measured
/// ~412-421ms with the resync line deleted, versus ~419-421ms with it
/// present -- statistically indistinguishable, both comfortably under the
/// 700ms bound below. This matches (and empirically confirms, at the
/// integration-test level) a same-day design-advisor correction to this
/// resync's own doc comment and to ADR-148 Amendment 8's Correction
/// paragraph: the sibling-COP-absorbs-unseen scenario this resync was
/// originally written to guard against is structurally unreachable in the
/// current architecture (every `bind_frame` call for a channel runs on
/// that channel's single `poll_channel_events` task -- no two wait loops on
/// one channel ever tick concurrently -- and the one real second poller, a
/// UUDT companion task, is CAN-only while concat requires KWP/J1850), so
/// every live `concat_segments_got` advance already reaches this loop's own
/// baseline via `check_match_against_baseline` before the resync line ever
/// runs -- the resync is defense-in-depth, not a live-race fix, and this
/// test's own single-COP construction cannot make it behave otherwise (nor
/// would a multi-COP variant: the correction shows the scenario is
/// unreachable by construction, not merely untested here). Kept anyway as a
/// coverage improvement: a finite, multi-match concat COP's total
/// completion time was previously untested by any test in this file.
#[tokio::test]
#[serial]
async fn concat_finite_multi_match_resyncs_baseline_between_windows() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Same window/units as `concat_is_multiple_completes_after_one_window_not_two`.
    let window_raw: u32 = 400_000;
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (j2534_0404::P2_MAX, window_raw),
            (CP_ENABLE_CONCATENATION, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    // Finite, NOT IS-MULTIPLE: two matches needed, so the receive phase must
    // loop around for a second window after match #1's own finalize.
    let cop = start_vacuous_send_recv(&mut client, cll_handle, 2).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Segment pair 1 (match #1 of 2): two frames, same SID, merge into one
    // buffer, finalized only by the shared CP_P2Max deadline expiring (no
    // differing-key frame to force an earlier mid-stream finalize).
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&[0x62, 0xAA]),
        j2534_0404::ISO14230,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&[0x62, 0xBB]),
        j2534_0404::ISO14230,
    );

    // Wait for match #1's own ResultData -- proves the first finalize (and
    // its resync line) already ran, without needing to inspect internal
    // state directly.
    let results = collect_result_data(&mut events, 1, 2_000).await;
    assert_eq!(results.len(), 1, "match #1's merged buffer must deliver");
    assert_eq!(results[0].data_bytes, vec![0x62, 0xAA, 0xBB]);

    // The COP must not be finished yet -- only 1 of 2 needed matches has
    // landed.
    assert!(
        !wait_for_event(&mut events, 150, |item| {
            is_finished(item)
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop.cop_handle)
        })
        .await,
        "the COP must still be waiting for match #2 -- only 1 of 2 needed matches has landed"
    );

    // Start the timing window right after match #1's own finalize. If the
    // resync were broken (baseline never advanced to the live value), the
    // NEXT poll would phantom-report the already-delivered segments as a
    // fresh `Absorbed`, restarting `CP_P2Max` an extra time before this
    // second window's own frames are ever considered -- adding a full
    // extra ~400ms window of latency.
    let start = tokio::time::Instant::now();

    // Segment pair 2 (match #2 of 2): two MORE frames, same SID,
    // distinguishable payload -- merges into its OWN fresh buffer (the
    // first buffer was already drained/finalized) and finalizes via its own
    // deadline expiry.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&[0x62, 0xCC]),
        j2534_0404::ISO14230,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&[0x62, 0xDD]),
        j2534_0404::ISO14230,
    );

    assert!(
        wait_for_event(&mut events, 3_000, |item| {
            is_finished(item)
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop.cop_handle)
        })
        .await,
        "the COP should finish once match #2's buffer is finalized by the window closing"
    );
    let elapsed = start.elapsed();
    // Mirrors `concat_is_multiple_completes_after_one_window_not_two`'s own
    // tight-bound calibration: ~20ms to absorb both segment-2 frames + one
    // ~400ms window + a small multiple of `POLL_INTERVAL_MS` (10ms) to
    // notice the deadline, roughly 450ms for the expected one-window
    // completion. 700ms sits comfortably above that but well under an
    // extra spurious window (~850ms), so a regression that reintroduced a
    // phantom `Absorbed` between the two matches would still be caught if
    // it (or a future, larger regression) pushed elapsed time up near or
    // past a second window -- a loose bound here would not actually
    // protect this test's own named behavior.
    assert!(
        elapsed < tokio::time::Duration::from_millis(700),
        "completion took {elapsed:?} after match #1's finalize, expected well under two ~400ms \
         windows -- a broken resync would phantom-restart CP_P2Max an extra time before match \
         #2's own frames are ever considered"
    );

    // fail-without/pass-with proof: see this fn's own module-level doc
    // comment above for the full result (the resync line was temporarily
    // deleted and this test re-run; it did NOT catch the regression, with
    // measured elapsed times and the reason why).

    drop(events);
    server.shutdown().await;
}

/// Coverage gap closed per edge-case-hunter review of PR #18 round 13: the
/// two `deliver_concat_batch_if_live_*` unit tests (`registrant_lifecycle_tests`)
/// exercise that helper directly with a hand-built `Vec<ConcatDelivery>`,
/// never through the real call site in `poll_rx_inner`'s per-frame loop
/// (`ctx.channel_id`/`entry.connect_generation` threaded from an actual poll
/// pass). Every other test in this file only reaches the DEADLINE-EXPIRY
/// finalize path (`finalize_and_deliver_concat_buffers_if_live`); none of
/// them ever trigger either of `bind_registrant`'s two INLINE force-finalize
/// arms (the empty-payload-match arm, or the byte/segment-cap-hit arm) that
/// feed `deliver_concat_batch_if_live` instead. This test exercises the
/// empty-payload-match arm specifically -- the cap-hit arm reaches the exact
/// same call site but needs a much more expensive payload to trigger.
///
/// `CP_EnableConcatenation = 1`, `num_receive_cycles: 1` (`matches_needed:
/// Some(1)`): the simplest case where finalizing the one open buffer alone
/// already satisfies the whole COP. Sequence: one non-empty segment opens a
/// fresh concat buffer (the "open new buffer" arm, which returns
/// `absorbed: true` -- it does NOT by itself complete the match, so the COP
/// is still waiting after this first frame); a second, EMPTY-payload frame
/// then matches the vacuous descriptor via `ExpectedResponse::matches`'s
/// `cmp_len == 0` short-circuit, force-finalizing the still-open buffer
/// (delivering segment 1's own content, unmerged with anything else) BEFORE
/// the empty match's own quota check runs -- which by then is already
/// satisfied by the finalize alone, so the empty frame's own match is never
/// separately counted.
///
/// This intentionally does NOT re-prove the disconnect-race-closing
/// property `deliver_concat_batch_if_live`'s own guard exists for -- the two
/// pure unit tests already cover that in isolation. Its only job is to
/// confirm the REAL call site (live `ctx.channel_id`/`entry.connect_generation`
/// wiring, not a hand-built batch) works end-to-end at all under normal
/// (non-disconnect) conditions; a regression back to the pre-round-13
/// unguarded direct `deliver_or_enqueue` call would still pass this test.
#[tokio::test]
#[serial]
async fn concat_empty_payload_match_finalizes_via_inline_guarded_delivery() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            (CP_ENABLE_CONCATENATION, 1),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let cop = start_vacuous_send_recv(&mut client, cll_handle, 1).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Opens a fresh concat buffer via the "open new buffer" arm -- absorbed,
    // not itself a completed match, so the COP keeps waiting.
    let segment_1 = vec![0x62, 0xAA, 0xBB];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&segment_1),
        j2534_0404::ISO14230,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Empty payload matches the vacuous descriptor via the `cmp_len == 0`
    // short-circuit -- force-finalizes the still-open buffer above (through
    // `deliver_concat_batch_if_live`'s real call site) before its own quota
    // check runs.
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &kwp_frame(&[]), j2534_0404::ISO14230);

    let results = collect_result_data(&mut events, 1, 2_000).await;
    assert_eq!(
        results.len(),
        1,
        "the finalized buffer must deliver exactly once"
    );
    assert_eq!(
        results[0].data_bytes, segment_1,
        "segment 1's own content, unmerged, since only one segment was ever absorbed into the \
         buffer before it was force-finalized"
    );

    assert!(
        wait_for_event(&mut events, 3_000, |item| {
            is_finished(item)
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == cop.cop_handle)
        })
        .await,
        "the COP must finish promptly via the inline force-finalize, without waiting for the \
         CP_P2Max deadline"
    );

    drop(events);
    server.shutdown().await;
}

/// Coverage gap closed per edge-case-hunter review of PR #18 round 14: no
/// test exercised the `skip_retransmit == true` short-circuit AT THE CALL
/// SITE in `wait_for_expected_response_inner`'s `0x21 | 0x23` arm -- the case
/// where a `CoptStopcomm` IS-MULTIPLE COP's `match_reset_ceiling` (ADR-087)
/// elapses before an RC21's own `request_time_ms` sleep completes, so
/// `discard_concat_buffers_for_retransmit_if_live` is never even called
/// (Rust's own `&&` short-circuit) -- distinct from the helper's OWN
/// stale-`connect_generation` reason for not discarding, which the three
/// `discard_concat_buffers_for_retransmit_if_live_*` unit tests already
/// cover. Combines `stopcomm_data_tx.rs`'s own
/// `stopcomm_is_multiple_rc21_request_time_bounded_by_match_reset_ceiling`
/// CEILING/RC21 setup (`CP_RC21RequestTime` deliberately longer than the ~2s
/// `match_reset_ceiling_ms` floor, forcing `skip_retransmit = true` before
/// the retransmit ever fires) with this file's own KWP CONCAT setup.
///
/// Sequence: one non-empty segment opens a concat buffer (the "open new
/// buffer" arm) via a vacuous descriptor; a single RC21 (NRC 0x21) frame then
/// arrives, intercepted by `detect_pending_rc` before the buffer's own key is
/// ever consulted (pending-RC detection runs unconditionally ahead of the
/// concat-matching logic in `bind_registrant`). Since `CP_RC21RequestTime`
/// (3s) outlives the ~2s ceiling, the chunked `request_time_ms` sleep's own
/// in-loop `ceiling_already_elapsed` check trips `skip_retransmit = true`, so
/// the helper is never called and the still-open buffer is left completely
/// untouched -- it must then survive to reach the ordinary deadline-expiry
/// finalize path (`deadline` is independently clamped to the same
/// `match_reset_ceiling` a few lines earlier in the same function) and be
/// delivered there, rather than being silently discarded.
///
/// Direct assertion used (buffer survives and is delivered), not the
/// narrower skip_retransmit-only fallback: investigation showed this
/// scenario's finalize timing is fully deterministic here -- once
/// `skip_retransmit` trips, the RC21 arm's own
/// `deadline = ceiling_slot.get_or_insert_with(..)` is immediately clamped
/// to `match_reset_ceiling` (a few lines below the skip), so the very next
/// per-pass deadline check fires the finalize with no extra window to race.
/// Confirmed by running this test in isolation multiple times: the segment
/// is delivered every time, comfortably inside the ~2.6s bound below.
#[tokio::test]
#[serial]
async fn concat_buffer_survives_rc21_skip_retransmit_at_ceiling() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[
            (j2534_0404::DATA_RATE, 10_400),
            // 50 ms window -> ceiling is the 2000 ms floor, same calibration
            // as `stopcomm_data_tx.rs`'s own reference test.
            (j2534_0404::P2_MAX, 50_000),
            (CP_ENABLE_CONCATENATION, 1),
            (CP_RC21_HANDLING, 1),
            (CP_RC_BYTE_OFFSET, 2),
            // 3000 ms request-time sleep (stored in us, ADR-057) --
            // deliberately longer than the ~2000 ms ceiling, so the ceiling
            // elapses mid-sleep and the retransmit (and therefore the
            // buffer-discard call) is skipped.
            (CP_RC21_REQUEST_TIME, 3_000_000),
        ],
    )
    .await;
    let mut events = subscribe(&mut client, cll_handle).await;

    let startcomm_cop = start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished_matching(&mut events, startcomm_cop).await;

    // CoptStopcomm, IS-MULTIPLE (`num_receive_cycles: -2`), with a vacuous
    // expected-response descriptor so the segment below can open a concat
    // buffer via the normal "open new buffer" arm.
    let final_payload = vec![0x82, 0xF1, 0x01];
    let stopcomm_cop = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptStopcomm as i32,
            cop_data: final_payload,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: -2,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    response_type: 0,
                    acceptance_id: 1,
                    mask_data: Vec::new(),
                    pattern_data: Vec::new(),
                    unique_resp_ids: vec![],
                }],
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, NumReceiveCycles = -2) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let start = tokio::time::Instant::now();

    // Segment 1: opens a fresh concat buffer via the "open new buffer" arm.
    let segment_1 = vec![0x62, 0xAA, 0xBB];
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&segment_1),
        j2534_0404::ISO14230,
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // A single NRC 0x21 (BusyRepeatRequest) at RC-byte-offset 2, echoing the
    // StopComm final message's own request SID (0x82) at byte 1 -- no
    // further frames are ever injected, so any second write to the mock
    // backdoor could only be the RC21 retransmit.
    server.backdoor.inject_rx(
        MOCK_CHANNEL_ID,
        &kwp_frame(&[0x7F, 0x82, 0x21]),
        j2534_0404::ISO14230,
    );

    // Direct assertion: the segment-1 buffer must survive the RC21
    // skip-retransmit short-circuit untouched and be delivered, unmerged,
    // once the deadline-expiry finalize path picks it up. Two `ResultData`
    // items arrive, not one: same convention
    // `stopcomm_is_multiple_rc21_request_time_bounded_by_match_reset_ceiling`
    // documents -- a pending-RC frame the RC21 handler consumes is *also*
    // surfaced to the CLL subscription as its own plain, undirected
    // `ResultData` (`acceptance_id == 0`), separate from any genuine COP
    // match. The concat buffer's own finalize delivery is distinguished by
    // `acceptance_id == 1`, this COP's own descriptor.
    let results = collect_result_data(&mut events, 2, 3_000).await;
    assert_eq!(
        results.len(),
        2,
        "expected the RC21 frame's own undirected delivery plus the surviving concat buffer's \
         finalize delivery"
    );
    assert!(
        results
            .iter()
            .any(|r| r.data_bytes == vec![0x7F, 0x82, 0x21] && r.acceptance_id == 0),
        "the RC21 frame itself must still surface as an undirected ResultData, same as the \
         reference test's own documented behavior"
    );
    let concat_result = results
        .iter()
        .find(|r| r.acceptance_id == 1)
        .expect("the concat buffer's own finalize delivery (acceptance_id 1) must be present");
    assert_eq!(
        concat_result.data_bytes, segment_1,
        "the surviving buffer's own content must be untouched by the skipped retransmit"
    );

    let elapsed = start.elapsed();
    assert!(
        elapsed <= tokio::time::Duration::from_millis(2_600),
        "elapsed {elapsed:?} should be close to the ~2s match_reset_ceiling, not stretch toward \
         the full 3s CP_RC21RequestTime"
    );

    assert!(
        wait_for_event(&mut events, 500, |item| {
            is_finished(item)
                && item
                    .cop_handle
                    .as_ref()
                    .is_some_and(|h| h.cop_handle == stopcomm_cop.cop_handle)
        })
        .await,
        "the IS-MULTIPLE COP must finish once the surviving buffer is finalized by the ceiling"
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

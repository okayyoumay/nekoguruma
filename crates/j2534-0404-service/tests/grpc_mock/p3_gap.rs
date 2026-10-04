//! `CP_P3Func`/`CP_P3Phys` minimum inter-request gap enforcement on CAN
//! (ADR-060): a functionally- or physically-addressed `CoptSendrecv` is
//! delayed until the configured gap has elapsed since the shared channel's
//! last send of the same addressing, when either that previous send or this
//! upcoming send required no response (`NumReceiveCycles == 0`).

use serial_test::serial;
use vci_service_interface::{ComPrimitiveCtrlData, ExpectedResponseData, StartComPrimitiveRequest};

use crate::harness::*;

const CP_P3_FUNC: u32 = 0x80B3;
const CP_P3_PHYS: u32 = 0x80B4;
const CP_REQUEST_ADDR_MODE: u32 = 0x8078;
const CP_CAN_FUNC_REQ_ID: u32 = 0x806B;

/// Starts a `CoptSendrecv` with explicit `NumSendCycles`/`NumReceiveCycles`
/// and returns immediately (does not wait for completion).
async fn start_send_recv(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
    cop_data: Vec<u8>,
    num_receive_cycles: i32,
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
                num_receive_cycles,
                temp_param_update: 0,
                expected_response_array: if num_receive_cycles == 0 {
                    vec![]
                } else {
                    vec![ExpectedResponseData {
                        response_type: 0,
                        acceptance_id: 0,
                        mask_data: vec![0x00],
                        pattern_data: vec![0x00],
                        unique_resp_ids: vec![],
                    }]
                },
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");
}

/// `CP_P3Phys`: the previous physically-addressed send required no response
/// (`NumReceiveCycles = 0`), so the next physically-addressed send must wait
/// at least `CP_P3Phys` since it.
#[tokio::test]
#[serial]
async fn phys_gap_enforced_when_previous_phys_send_required_no_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000), (CP_P3_PHYS, 300_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    start_send_recv(&mut client, cll_handle, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    start_send_recv(&mut client, cll_handle, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 0, 1);

    assert!(
        elapsed >= std::time::Duration::from_millis(280),
        "the second physical send should wait ~300ms for CP_P3Phys (waited {elapsed:?})"
    );

    server.shutdown().await;
}

/// `CP_P3Phys`: when the previous physically-addressed send *did* require a
/// response, no artificial gap is enforced -- the next send goes out
/// immediately.
#[tokio::test]
#[serial]
async fn phys_gap_not_enforced_when_previous_phys_send_required_a_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_P3_PHYS, 300_000),
            (j2534_0404::P2_MAX, 50_000),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // First send requires a response; none is ever injected, so it ends in
    // an (expected, ignored) receive timeout after CP_P2Max -- but it *did*
    // require one, so it must not seed the gap-enforcement bucket.
    start_send_recv(&mut client, cll_handle, vec![0x3E, 0x00], 1).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    start_send_recv(&mut client, cll_handle, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 0, 1);

    assert!(
        elapsed < std::time::Duration::from_millis(280),
        "no CP_P3Phys gap should be enforced when the previous send required a response (waited {elapsed:?})"
    );

    server.shutdown().await;
}

/// `CP_P3Func`: even when the *previous* functionally-addressed send
/// required a response, the gap is still enforced before the *next*
/// functional send when that upcoming send itself requires no response
/// (`NumReceiveCycles = 0`) -- the trigger is either side of the pair, not
/// only the previous one (ADR-060).
#[tokio::test]
#[serial]
async fn func_gap_enforced_when_upcoming_func_send_requires_no_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_P3_FUNC, 300_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
            (j2534_0404::P2_MAX, 50_000),
        ],
    )
    .await;

    // First functional send requires a response (never injected, times out
    // after CP_P2Max) -- it does NOT set no_response_required on its own.
    start_send_recv(&mut client, cll_handle, vec![0x01, 0x00], 1).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Second functional send requires no response itself -- this alone must
    // trigger the CP_P3Func gap even though the previous send required one.
    start_send_recv(&mut client, cll_handle, vec![0x01, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 0, 1);

    assert!(
        elapsed >= std::time::Duration::from_millis(280),
        "the second functional send should wait ~300ms for CP_P3Func because it itself requires no response (waited {elapsed:?})"
    );

    server.shutdown().await;
}

/// `CP_P3Func`: when both the previous and the upcoming functionally-
/// addressed sends require a response, no artificial gap is enforced.
#[tokio::test]
#[serial]
async fn func_gap_not_enforced_when_both_func_sends_require_a_response() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_P3_FUNC, 300_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
            (j2534_0404::P2_MAX, 50_000),
        ],
    )
    .await;

    start_send_recv(&mut client, cll_handle, vec![0x01, 0x00], 1).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    start_send_recv(&mut client, cll_handle, vec![0x01, 0x00], 1).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 0, 1);

    assert!(
        elapsed < std::time::Duration::from_millis(280),
        "no CP_P3Func gap should be enforced when both sends require a response (waited {elapsed:?})"
    );

    server.shutdown().await;
}

/// `CP_P3Phys` gap tracking is scoped to the shared physical channel, not to
/// a single CLL: a physical send from one CLL sharing the channel still
/// delays the next physical send from a *different* CLL on that same
/// channel.
#[tokio::test]
#[serial]
async fn phys_gap_is_scoped_to_the_shared_channel_across_clls() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000), (CP_P3_PHYS, 300_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;

    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000), (CP_P3_PHYS, 300_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E1).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_a and cll_b should share one physical channel"
    );
    // cll_b is a joining CLL on an already-open channel: its Active set stays
    // default (all zero) until it issues CoptUpdateparam to push its own
    // Working params (including CP_P3_PHYS) across.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: Vec::new(),
            cop_ctrl_data: None,
        })
        .await
        .expect("CoptUpdateparam should succeed for cll_b");

    start_send_recv(&mut client, cll_a, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    start_send_recv(&mut client, cll_b, vec![0x3E, 0x00], 0).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 0, 1);

    assert!(
        elapsed >= std::time::Duration::from_millis(280),
        "CLL B's physical send should still wait for CP_P3Phys against CLL A's send on the same shared channel (waited {elapsed:?})"
    );

    server.shutdown().await;
}

/// Regression test for the `CP_TesterPresentReqRsp`/`TxGapState`
/// classification fix folded into the mode-0/mode-1 tester-present
/// unification diff (§5 of the design brief): a tester-present send's own
/// `no_response_required` `TxGapState` stamp is now derived from
/// `CP_TesterPresentReqRsp` (`!expects_response`) instead of being
/// hardcoded to `true` regardless of whether that tester-present send
/// itself expects a response.
///
/// `CP_TesterPresentReqRsp = 1`: the arm-time tester-present send (ADR-084)
/// expects a response, so it must NOT be classified as
/// `no_response_required` for `CP_P3Phys` purposes -- a subsequent physical
/// `CoptSendrecv` on the same addressing bucket must NOT be gap-blocked by
/// it. Contrast: `tester_present_reqrsp_0_send_still_gap_blocks_subsequent_sendrecv`.
#[tokio::test]
#[serial]
async fn tester_present_reqrsp_1_send_does_not_gap_block_subsequent_sendrecv() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000), (CP_P3_PHYS, 300_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    // Large interval: only the immediate arm-time send (ADR-084) matters
    // here, not any later due-triggered re-send.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    // CoptStartcomm's own arm-time send is the tester-present send whose
    // CP_P3Phys classification is under test.
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
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90], 1).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 0, 1);

    assert!(
        elapsed < std::time::Duration::from_millis(280),
        "a CoptSendrecv following a CP_TesterPresentReqRsp = 1 tester-present send should not be \
         gap-blocked by CP_P3Phys -- that send correctly expects a response, so it must not be \
         classified as no_response_required (waited {elapsed:?})"
    );

    server.shutdown().await;
}

/// Contrast case for `tester_present_reqrsp_1_send_does_not_gap_block_subsequent_sendrecv`:
/// `CP_TesterPresentReqRsp = 0` (the default) -- the arm-time tester-present
/// send does NOT expect a response, so it is correctly classified as
/// `no_response_required` and a subsequent physical `CoptSendrecv` on the
/// same addressing bucket is still gap-blocked by `CP_P3Phys`, exactly as
/// before this diff.
#[tokio::test]
#[serial]
async fn tester_present_reqrsp_0_send_still_gap_blocks_subsequent_sendrecv() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000), (CP_P3_PHYS, 300_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
    // CP_TesterPresentReqRsp is left at its default (0).
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

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
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    start_send_recv(&mut client, cll_handle, vec![0x22, 0xF1, 0x90], 1).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 0, 1);

    assert!(
        elapsed >= std::time::Duration::from_millis(280),
        "a CoptSendrecv following a CP_TesterPresentReqRsp = 0 tester-present send should still \
         be gap-blocked by CP_P3Phys (waited {elapsed:?})"
    );

    server.shutdown().await;
}

/// Pins `dispatch_due_tester_present`'s own due-triggered per-tick send stamp
/// (`send_tester_present_once`'s `no_response_required: !expects_response`,
/// fed by the `still_due` recheck's fresh `CP_TesterPresentReqRsp` read --
/// `events.rs`): mode 0 (periodic), physical addressing,
/// `CP_TesterPresentReqRsp = 1` -- every due-triggered tester-present send
/// correctly stamps `no_response_required = false`, so `CP_P3Phys`'s
/// "previous send required no response" gate never triggers between
/// consecutive due-triggered sends, which instead fire back-to-back at the
/// configured `CP_TesterPresentTime` cadence (~50ms), well under the 300ms
/// `CP_P3Phys` gap. Pre-fix, this stamp was hardcoded to
/// `no_response_required: true` regardless of `expects_response`, which would
/// have wrongly gapped every due-triggered send to ~300ms.
#[tokio::test]
#[serial]
async fn tester_present_reqrsp_1_due_sends_are_not_phys_gapped_from_each_other() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000), (CP_P3_PHYS, 300_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 50_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

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
    // ADR-084: the arm-time send itself, immediate and synchronous.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // First due-triggered send (~50ms after the arm-time send above).
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    // Second due-triggered send: if it were wrongly gated by CP_P3Phys, this
    // would take ~300ms instead of the configured ~50ms interval.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 1, 2);

    assert!(
        elapsed < std::time::Duration::from_millis(280),
        "consecutive due-triggered tester-present sends should fire on the ~50ms \
         CP_TesterPresentTime cadence, not be CP_P3Phys-gapped, when CP_TesterPresentReqRsp = 1 \
         (waited {elapsed:?})"
    );

    server.shutdown().await;
}

/// Pins `dispatch_due_tester_present`'s own `wait_for_p3_gap` call argument
/// (`if snapshotted_expects_response { 1 } else { 0 }`, replacing a
/// pre-fix hardcoded literal `0` -- `events.rs`): functional CAN addressing,
/// `CP_TesterPresentReqRsp = 1` -- neither the previous due-triggered send's
/// own stamp (always `no_response_required = false` when
/// `CP_TesterPresentReqRsp = 1`, see
/// `tester_present_reqrsp_1_due_sends_are_not_phys_gapped_from_each_other`)
/// nor the upcoming send's own `this_requires_no_response` (correctly `false`
/// here, since the passed `num_receive_cycles` is `1`, not a hardcoded `0`)
/// triggers `CP_P3Func`'s "upcoming send requires no response" clause, so
/// consecutive due-triggered sends fire on the configured
/// `CP_TesterPresentTime` cadence instead of being gapped to `CP_P3Func`.
#[tokio::test]
#[serial]
async fn tester_present_reqrsp_1_due_sends_are_not_func_gapped_from_each_other() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_P3_FUNC, 300_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
            (CP_TESTER_PRESENT_ADDR_MODE, 1),
        ],
    )
    .await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 50_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

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
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 1, 2);

    assert!(
        elapsed < std::time::Duration::from_millis(280),
        "consecutive due-triggered tester-present sends should fire on the ~50ms \
         CP_TesterPresentTime cadence, not be CP_P3Func-gapped, when CP_TesterPresentReqRsp = 1 \
         (waited {elapsed:?})"
    );

    server.shutdown().await;
}

/// Contrast/control case for
/// `tester_present_reqrsp_1_due_sends_are_not_func_gapped_from_each_other`:
/// `CP_TesterPresentReqRsp = 0` (its default) -- every due-triggered send
/// requires no response, so `this_requires_no_response` is `true` regardless
/// of the fix under test (the passed `num_receive_cycles` argument is `0`
/// either way when `CP_TesterPresentReqRsp = 0`), and `CP_P3Func`'s gap is
/// (correctly) enforced between consecutive due-triggered sends, exactly as
/// before this diff. Included for completeness/contrast, not as a
/// revert-proof: this is the pre-existing, already-correct behavior for the
/// no-response case.
#[tokio::test]
#[serial]
async fn tester_present_reqrsp_0_due_sends_are_func_gapped_from_each_other() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_P3_FUNC, 300_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
            (CP_TESTER_PRESENT_ADDR_MODE, 1),
        ],
    )
    .await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 50_000).await;
    // CP_TesterPresentReqRsp is left at its default (0).
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

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
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 1, 2);

    assert!(
        elapsed >= std::time::Duration::from_millis(280),
        "consecutive due-triggered tester-present sends should still be CP_P3Func-gapped when \
         CP_TesterPresentReqRsp = 0 (waited {elapsed:?})"
    );

    server.shutdown().await;
}

/// The actual finding's mechanism (design brief's "the accepted one-send
/// residual"): mode 0, physical addressing, `CP_TesterPresentReqRsp` starts
/// at `1` (so consecutive due-triggered sends fire ~50ms apart, unblocked by
/// `CP_P3Phys`, per
/// `tester_present_reqrsp_1_due_sends_are_not_phys_gapped_from_each_other`).
/// A `CoptUpdateparam` mid-stream flips it to `0` -- `ResolvedTesterPresent::
/// same_wire_behavior` deliberately excludes `expects_response` from its
/// comparison (`rpc_primitive.rs`), so this does NOT re-arm/resend
/// tester-present; the change only becomes visible through
/// `dispatch_due_tester_present`'s `still_due` recheck's own fresh
/// `CP_TesterPresentReqRsp` read on the very next per-tick send, not at
/// arm/re-arm time. One due-triggered send may straddle the update itself
/// (its own outcome is a race by construction and is not asserted on here);
/// the FOLLOWING consecutive pair of due-triggered sends, once the flip has
/// definitely had a chance to take effect, is `CP_P3Phys`-gapped to ~300ms --
/// proving the `still_due` recheck's live re-read, not just arm/re-arm-time
/// resolution, is what lets a `CP_TesterPresentReqRsp` change take effect on
/// the very next per-tick send.
#[tokio::test]
#[serial]
async fn tester_present_reqrsp_live_flip_takes_effect_on_next_due_send() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000), (CP_P3_PHYS, 300_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 50_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

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
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Two due-triggered sends while CP_TesterPresentReqRsp is still 1: fire
    // back-to-back at the ~50ms cadence, not CP_P3Phys-gapped (mirrors
    // tester_present_reqrsp_1_due_sends_are_not_phys_gapped_from_each_other).
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 1, 2);
    assert!(
        elapsed < std::time::Duration::from_millis(280),
        "the due-triggered sends before the flip should not be CP_P3Phys-gapped (waited {elapsed:?})"
    );

    // Flip CP_TesterPresentReqRsp live, via CoptUpdateparam -- issued directly
    // (not via promote_via_update_param, whose own ~100ms settling sleep
    // would confound the immediately-following written_count check below).
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 0).await;
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: Vec::new(),
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptUpdateparam) should succeed");

    // ResolvedTesterPresent::same_wire_behavior excludes expects_response, so
    // this CoptUpdateparam must not have re-armed/resent tester-present --
    // written_count must be unchanged immediately after the call returns (a
    // re-arm's own send happens synchronously, before CoptUpdateparam
    // completes, same as every other arm/re-arm site in this service).
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        3,
        "a CP_TesterPresentReqRsp-only CoptUpdateparam must not re-arm/resend tester-present"
    );

    // The very next due-triggered send may straddle the flip (a race by
    // construction, per the design brief) -- wait for it, but assert nothing
    // about its own timing.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 4).await;

    // The FOLLOWING consecutive pair, once the flip has definitely had a
    // chance to take effect (via the still_due recheck's fresh read), should
    // be CP_P3Phys-gapped to ~300ms -- proving the live re-read, not just
    // arm/re-arm-time resolution, is what lets the change take effect.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 5).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 6).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 4, 5);
    assert!(
        elapsed >= std::time::Duration::from_millis(280),
        "the due-triggered sends after the CP_TesterPresentReqRsp flip has taken effect should be \
         CP_P3Phys-gapped (waited {elapsed:?})"
    );

    server.shutdown().await;
}

/// Codex review finding on PR #97 (ADR-093 follow-up): `dispatch_due_
/// tester_present`'s due-snapshot critical section used to capture
/// `expects_response` from `resolved.expects_response` --
/// `TesterPresentState::Armed`'s own `resolved: ResolvedTesterPresent` field,
/// populated once at arm/re-arm time and never refreshed by a
/// `CP_TesterPresentReqRsp`-only `CoptUpdateparam` (`same_wire_behavior`
/// deliberately excludes `expects_response`, ADR-088, since it is RX-
/// classification, not wire content). That made the due-snapshot's copy
/// stale indefinitely, not just for one race window, unlike the `still_due`
/// recheck a bit further down (which already reads `CP_TesterPresentReqRsp`
/// live via `l.active.tester_present_req_rsp()`). Fixed by reading
/// `link.active.tester_present_req_rsp()` live in the due-snapshot too.
///
/// This test isolates the due-snapshot's OWN value from `still_due`'s
/// already-correct one: mode 0, functional CAN addressing, `CP_P3Func` set to
/// a real gap. `CP_TesterPresentReqRsp` starts at `0`, so the initial
/// due-triggered sends are (correctly, both pre- and post-fix)
/// `CP_P3Func`-gapped, mirroring
/// `tester_present_reqrsp_0_due_sends_are_func_gapped_from_each_other`. A
/// `CoptUpdateparam` then flips `CP_TesterPresentReqRsp` to `1`, well ahead of
/// the CLL's own ~50ms `CP_TesterPresentTime` interval elapsing again; per
/// `same_wire_behavior`, this does not re-arm/resend tester-present, so
/// `resolved.expects_response` is never refreshed.
///
/// `CP_P3Func`'s gate is `prev.no_response_required || this_requires_no_
/// response`, so two due-triggered sends after the flip are needed: the
/// FIRST clears `prev.no_response_required` (via `still_due`'s own,
/// separately-already-correct, live read used to stamp the actual send) but
/// is itself still gapped by the PRE-flip previous send's stale `true` stamp,
/// regardless of this fix -- not asserted on here. By the SECOND, `prev` is
/// now `false`, so the gate depends purely on the due-snapshot's own
/// `this_requires_no_response` -- exactly the value this fix corrects. Pre-fix,
/// the due-snapshot keeps reading the frozen `resolved.expects_response =
/// false` from the `CP_TesterPresentReqRsp = 0` arm forever, forcing
/// `this_requires_no_response = true` and gapping that second send to
/// ~300ms; post-fix, it reads the live (now `1`) value, so the send is not
/// gapped and fires promptly on the ~50ms cadence.
#[tokio::test]
#[serial]
async fn tester_present_reqrsp_due_snapshot_reads_live_not_frozen_resolved() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_P3_FUNC, 300_000),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
            (CP_TESTER_PRESENT_ADDR_MODE, 1),
        ],
    )
    .await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 50_000).await;
    // CP_TesterPresentReqRsp starts at its default (0).
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

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
    // ADR-084: the arm-time send itself, immediate and synchronous.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // First due-triggered send, while CP_TesterPresentReqRsp is still 0: both
    // pre- and post-fix agree here (this_requires_no_response = true either
    // way), so it is CP_P3Func-gapped -- not asserted, just waited out, to
    // seed last_func_tx with a `no_response_required = true` stamp.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;

    // Flip CP_TesterPresentReqRsp live, via CoptUpdateparam -- issued directly
    // (not via promote_via_update_param's own settling sleep), well ahead of
    // the ~50ms CP_TesterPresentTime interval elapsing again.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_REQ_RSP, 1).await;
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: Vec::new(),
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptUpdateparam) should succeed");

    // same_wire_behavior excludes expects_response, so this CoptUpdateparam
    // must not have re-armed/resent tester-present.
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "a CP_TesterPresentReqRsp-only CoptUpdateparam must not re-arm/resend tester-present"
    );

    // First due-triggered send after the flip: still CP_P3Func-gapped
    // regardless of this fix (the PRE-flip previous send's stale
    // no_response_required = true stamp alone forces the gate), but its own
    // still_due-driven stamp clears prev.no_response_required for the next
    // one. Not asserted on timing.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 3).await;

    // The SECOND due-triggered send after the flip is the one under test:
    // with prev.no_response_required now false, the gate depends solely on
    // the due-snapshot's own this_requires_no_response.
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 4).await;
    let elapsed = server.backdoor.written_gap(MOCK_CHANNEL_ID, 2, 3);

    assert!(
        elapsed < std::time::Duration::from_millis(280),
        "the second due-triggered send after the CP_TesterPresentReqRsp flip should not be \
         CP_P3Func-gapped -- the due-snapshot itself must read the live CP_TesterPresentReqRsp \
         value, not the frozen resolved.expects_response from the reqrsp=0 arm (waited {elapsed:?})"
    );

    server.shutdown().await;
}

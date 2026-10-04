//! ADR-067: ComParams bind to a ComPrimitive at `StartComPrimitive` call
//! time (a snapshot), not live at poll-task execution time. Covers:
//!
//! - Claim 9: a `CoptSendrecv`/`CoptStartcomm` uses the ComParam values bound
//!   at its own call time for its whole life, immune to a later
//!   `SetComParam` (pinned deterministically via a long `CoptDelay` that
//!   holds the FIFO queue open across every relevant RPC call).
//! - Claim 4: `CoptUpdateparam` snapshots Working at its own call time; a
//!   later `SetComParam` is not promoted by it.
//! - Claim 3/E: the `PDU_PC_BUSTYPE` `temp_param_update` guard
//!   (`PDU_ERR_TEMPPARAM_NOT_ALLOWED`) rejects synchronously, with no
//!   enqueue/writeback/side effects.
//! - Claim 2/D: `temp_param_update=1` writes Working back from Active after
//!   a successful call, for every temp-eligible COP type including
//!   `CoptStopcomm`.
//! - The temp hardware REVERT reads the live Active set at revert time (the
//!   one deliberate exception to call-time binding): a `CoptUpdateparam`
//!   queued ahead of the temp COP must not be undone by the revert.

use serial_test::serial;
use vci_service_interface::{
    ComPrimitiveCtrlData, ExpectedResponseData, StartComPrimitiveRequest, event_item,
    subscribe_event_request,
};

use crate::harness::*;

/// Waits for the next `PduCopstFinished` on an already-open event stream
/// (must be subscribed BEFORE the COP being waited on is started).
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

/// Claim 9: a `temp_param_update=1` `CoptSendrecv` binds `effective` (the
/// Working snapshot) at ITS OWN `StartComPrimitive` call time. A long
/// `CoptDelay` queued first holds the FIFO open across every RPC call this
/// test makes, so the ordering is structural (not timing-lucky, unlike the
/// in-process-harness caveat ADR-063/064 noted): the `SetComParam` issued
/// immediately after the `CoptSendrecv` call is guaranteed to still be
/// sitting unprocessed in front of it when the send is enqueued, yet must
/// have no effect on it once the queue finally drains.
#[tokio::test]
#[serial]
async fn claim9_sendrecv_pins_working_snapshot_at_call_time_despite_later_set_com_param() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // Stage (Working only) a switch to functional addressing with a first
    // functional CAN ID -- this is what the temp COP below must bind.
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;

    // Hold the FIFO queue open for long enough that every RPC call below is
    // guaranteed to complete (and enqueue its TxItem) before any of them
    // actually execute.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
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

    // Queued behind the Delay: the COP under test. Its temp_param_update
    // snapshot (CP_CanFuncReqId = 0x7DF) is bound synchronously, right now.
    let payload = vec![0x02, 0x10, 0x03];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: payload.clone(),
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv, temp_param_update=1) should succeed");

    // Immediately after -- while both the Delay and the SendRecv above are
    // still sitting unprocessed in the FIFO queue -- change Working's
    // functional CAN ID to a second value. If resolution were live (as
    // before ADR-067), the already-enqueued SendRecv would pick this up when
    // its turn eventually comes; bound at call time, it must not.
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DE).await;

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&payload);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected,
        "the temp CoptSendrecv must use CP_CanFuncReqId=0x7DF, the value bound at its own call \
         time -- not 0x7DE, staged by a SetComParam issued afterward while the COP was still \
         queued behind the CoptDelay"
    );

    server.shutdown().await;
}

/// Claim 4: `CoptUpdateparam` snapshots Working at ITS OWN call time; a
/// `SetComParam` issued after that call must not be promoted by it, even
/// though (thanks to a long `CoptDelay` holding the queue open) the
/// `SetComParam` is guaranteed to land well before `CoptUpdateparam` actually
/// executes.
#[tokio::test]
#[serial]
async fn claim4_updateparam_pins_working_snapshot_at_call_time_despite_later_set_com_param() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::BIT_SAMPLE_POINT, 80),
        ],
    )
    .await;
    // Active is 80 (pushed at connect); stage a new Working value.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::BIT_SAMPLE_POINT, 90).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Hold the FIFO queue open across every RPC call below.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
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

    // Queued behind the Delay: CoptUpdateparam snapshots Working (90) now.
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

    // Stage a THIRD value in Working, after CoptUpdateparam's call already
    // captured its snapshot. Must not be promoted by it.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::BIT_SAMPLE_POINT, 123).await;

    // Drain: CoptDelay finishes first, then CoptUpdateparam.
    wait_for_cop_finished(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BIT_SAMPLE_POINT),
        90,
        "Active must reflect the value at CoptUpdateparam's call time (90), not the later \
         SetComParam (123)"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::BIT_SAMPLE_POINT).await,
        123,
        "Working keeps the later SetComParam's value -- CoptUpdateparam does not touch Working"
    );

    drop(events);
    server.shutdown().await;
}

/// Claim 3/E: staging a `PDU_PC_BUSTYPE`-class ComParam change in Working
/// (here, `CP_SyncJumpWidth`) and then calling `StartComPrimitive` with
/// `temp_param_update=1` must be rejected synchronously with
/// `PDU_ERR_TEMPPARAM_NOT_ALLOWED`, with no enqueue, no writeback, and no
/// side effects -- for both `CoptSendrecv` and `CoptStartcomm`.
#[tokio::test]
#[serial]
async fn bustype_guard_rejects_temp_param_update_for_sendrecv_and_startcomm() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // CP_SyncJumpWidth (BUSTYPE class) differs between Working (15, just
    // staged) and Active (unset/0, never promoted by CoptUpdateparam).
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::SYNC_JUMP_WIDTH, 15).await;

    let baseline_set_config = server.backdoor.set_config_count();

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x02, 0x10, 0x03],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect_err("CoptSendrecv temp_param_update should be rejected by the BUSTYPE guard");
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status.message().contains("PDU_ERR_TEMPPARAM_NOT_ALLOWED"),
        "{}",
        status.message()
    );

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect_err("CoptStartcomm temp_param_update should be rejected by the BUSTYPE guard");
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status.message().contains("PDU_ERR_TEMPPARAM_NOT_ALLOWED"),
        "{}",
        status.message()
    );

    // No side effects: nothing was ever enqueued (no message written, no
    // extra SET_CONFIG call), and Working was not written back from Active.
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);
    assert_eq!(server.backdoor.set_config_count(), baseline_set_config);
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::SYNC_JUMP_WIDTH).await,
        15
    );

    server.shutdown().await;
}

/// Same guard as `bustype_guard_rejects_temp_param_update_for_sendrecv_and_startcomm`,
/// but triggered via `CP_Parity` specifically (ADR-110 amendment, Codex
/// review Finding 1 on PR #116) rather than `CP_SyncJumpWidth` -- `CP_Parity`
/// joined `BUSTYPE_UNUM32` alongside every other member, since its physical
/// effect (writing the native J2534 `PARITY` config) is exactly as
/// unambiguous as `CP_UartConfig`'s, even though its own ISO `PDU_PC_BUSTYPE`
/// *label* was historically excluded (ADR-067) for an unrelated naming
/// reason. Both consumers (`apply_bustype_lock`'s hardware-safety exclusion
/// and this synchronous `bustype_params_differ` guard) iterate the same
/// `BUSTYPE_UNUM32` const, so this pins that `CP_Parity` is covered by BOTH,
/// not just whichever member an existing test happened to exercise.
#[tokio::test]
#[serial]
async fn bustype_guard_rejects_temp_param_update_for_cp_parity() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // CP_Parity is only allowlisted for the KWP family (ISO9141/ISO14230),
    // not CAN (`comparam_support::is_kwp_param`) -- unlike the
    // CP_SyncJumpWidth variant of this test, no addressing setup is needed:
    // the BUSTYPE guard is checked before any TX/addressing resolution.
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO9141,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    // CP_Parity (BUSTYPE class as of ADR-110) differs between Working (1,
    // just staged) and Active (unset/0, never promoted by CoptUpdateparam).
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::PARITY, 1).await;

    let baseline_set_config = server.backdoor.set_config_count();

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x02, 0x10, 0x03],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect_err("CoptSendrecv temp_param_update should be rejected by the BUSTYPE guard");
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status.message().contains("PDU_ERR_TEMPPARAM_NOT_ALLOWED"),
        "{}",
        status.message()
    );

    // No side effects: nothing was ever enqueued (no message written, no
    // extra SET_CONFIG call), and Working was not written back from Active.
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);
    assert_eq!(server.backdoor.set_config_count(), baseline_set_config);
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::PARITY).await,
        1
    );

    server.shutdown().await;
}

/// The temp hardware revert reads the LIVE Active set at revert time -- not
/// the Active snapshot from the temp COP's own call time. A `CoptUpdateparam`
/// called BEFORE the temp COP (so the temp COP's call-time Active snapshot
/// predates it) but executed after the temp COP's call (both held in the FIFO
/// behind a long `CoptDelay`) legitimately moves hardware and Active to new
/// values first; the temp COP's revert must leave those in place, not roll
/// hardware back to its stale call-time snapshot (which would silently
/// diverge hardware from the Active buffer the next plain COP resolves from).
#[tokio::test]
#[serial]
async fn temp_revert_targets_live_active_not_call_time_snapshot() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Active (pushed at connect): LOOPBACK=0. LOOPBACK is a hardware
    // SET_CONFIG param that is NOT PDU_PC_BUSTYPE-class, so it may differ
    // Working-vs-Active without tripping the temp guard.
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000), (j2534_0404::LOOPBACK, 0)],
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

    // Hold the FIFO queue open across every RPC call below.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
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

    // Stage LOOPBACK=1 in Working, then queue the CoptUpdateparam that will
    // promote it (hardware + Active) when the queue drains.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::LOOPBACK, 1).await;
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

    // The temp COP, called AFTER the CoptUpdateparam call but BEFORE it
    // executes: its call-time Active snapshot still has LOOPBACK=0 (stale by
    // the time it runs), while its `effective` (Working) carries LOOPBACK=1.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x02, 0x10, 0x03],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptSendrecv, temp_param_update=1) should succeed");

    // Drain: Delay, then UpdateParam (hardware+Active -> LOOPBACK=1), then
    // the temp SendRecv (apply effective, transmit, revert).
    wait_for_cop_finished(&mut events).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::LOOPBACK),
        1,
        "the temp COP's revert must restore the LIVE Active set (LOOPBACK=1, promoted by the \
         CoptUpdateparam that executed ahead of it) -- not its own stale call-time Active \
         snapshot (LOOPBACK=0), which would leave hardware diverged from the Active buffer"
    );

    drop(events);
    server.shutdown().await;
}

/// Claim 2/D: a successful `temp_param_update=1` call writes Working back
/// from Active, even for `CoptStopcomm` -- which performs no hardware writes
/// of its own and was, before ADR-067, entirely unaffected by the flag.
///
/// Uses `CP_RC21Handling` (`PDU_PC_SPECIFIED`-class, neither BUSTYPE nor
/// TESTER_PRESENT) rather than the TESTER_PRESENT-class `CP_TesterPresentTime`
/// this test originally staged: since ADR-133's Codex-review-round fix (PR
/// #147), staging a TESTER_PRESENT-class Working/Active difference and then
/// calling `temp_param_update=1` is itself rejected with
/// `PDU_ERR_TEMPPARAM_NOT_ALLOWED` (ISO 22900-2 §9.4.16.2.1 f)) -- the exact
/// scenario `locks_and_param_classes.rs`'s
/// `tester_present_guard_rejects_temp_param_update_for_sendrecv_and_startcomm`
/// now pins -- so it can no longer also serve as this claim's writeback
/// fixture.
#[tokio::test]
#[serial]
async fn stopcomm_temp_param_update_writes_working_back_from_active() {
    const CP_RC21_HANDLING: u32 = 0x8021;

    let server = TestServer::start().await;
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

    // Start comm (plain) so CoptStopcomm is valid.
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
    wait_for_cop_finished(&mut events).await;

    // Stage a Working-only change (Active never had CP_RC21Handling set).
    set_com_param_unum32(&mut client, cll_handle, CP_RC21_HANDLING, 100).await;
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_RC21_HANDLING).await,
        100
    );

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStopcomm, temp_param_update=1) should succeed");
    wait_for_cop_finished(&mut events).await;

    // Working now equals Active (0 -- CP_RC21Handling was never promoted to
    // Active), discarding the staged 100.
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_RC21_HANDLING).await,
        0
    );

    drop(events);
    server.shutdown().await;
}

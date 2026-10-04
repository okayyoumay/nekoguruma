//! ADR-068: the UniqueRespIdTable gets a Working/Active split, mirroring the
//! `working`/`active` ComParamSet split (ADR-067), with one asymmetry: a COP
//! always snapshots the ACTIVE table at `StartComPrimitive` call time,
//! regardless of `temp_param_update` -- there is no Working-side table
//! binding for a COP to borrow (ADR-067 claim G is unchanged by this ADR).
//! `SetUniqueRespIdTable` stages Working only; promotion (and the ISO15765
//! `FLOW_CONTROL_FILTER` reconciliation that goes with it) happens at this
//! CLL's own `ConnectComLogicalLink` or at `CoptUpdateparam` execution.
//!
//! Covers:
//! - `GetUniqueRespIdTable` reads Working (staged) values before promotion.
//! - A `CoptSendrecv` uses the OLD Active table (the snapshot at its own
//!   call time), not a Working table staged but not yet promoted; a
//!   following `CoptUpdateparam` promotes the staged table for the NEXT
//!   `CoptSendrecv`.
//! - `temp_param_update=1` does not change which table a COP reads: still
//!   ACTIVE, never the staged Working table.
//! - A temp COP's completion (Working := Active writeback, ADR-067 claim D)
//!   never touches the UniqueRespIdTable: a staged Working table survives.
//! - `CoptRestoreParam` copies Active back into Working for the table too,
//!   with no filter I/O (Active is unchanged).
//! - `CoptUpdateparam` with an unchanged table (only a plain ComParam value
//!   changed) does no `FLOW_CONTROL_FILTER` stop/start -- the promotion
//!   helper's diff gate.

use serial_test::serial;
use vci_service_interface::{
    ComPrimitiveCtrlData, ExpectedResponseData, PduError, SetUniqueRespIdTableRequest,
    StartComPrimitiveRequest, UniqueRespIdTableItem, error_detail_from_status, event_item,
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

/// `GetUniqueRespIdTable` always reads the Working table, immediately after
/// `SetUniqueRespIdTable` -- before anything has promoted it to Active.
#[tokio::test]
#[serial]
async fn get_unique_resp_id_table_returns_working_before_promotion() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(1, vec![unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0)])],
    )
    .await;

    // No CoptUpdateparam has run yet, and no hardware filter reflects this
    // table -- but Get still returns it (Working), same as GetComParam.
    let table = get_unique_resp_table(&mut client, cll_handle).await;
    assert_eq!(table.len(), 1);
    assert_eq!(table[0].unique_resp_identifier, 1);
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        0,
        "Set alone must not touch hardware filters"
    );

    server.shutdown().await;
}

/// A `CoptSendrecv` binds the ACTIVE UniqueRespIdTable at its own
/// `StartComPrimitive` call time. Setting a new table afterward (staged in
/// Working only) does not affect a send already resolved against the OLD
/// Active table; a following `CoptUpdateparam` promotes the staged table so
/// the NEXT `CoptSendrecv` uses it.
#[tokio::test]
#[serial]
async fn sendrecv_uses_old_active_table_until_updateparam_promotes_the_new_one() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // Establish an initial Active table (0x7E0).
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(1, vec![unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0)])],
    )
    .await;

    // Stage a DIFFERENT table (0x7DF) in Working only -- do not promote yet.
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(1, vec![unum32_param(CP_CAN_PHYS_REQ_ID, 0x7DF)])],
    )
    .await;

    // This send's call-time Active snapshot is still the OLD table (0x7E0).
    let payload = vec![0x02, 0x10, 0x03];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    let mut expected_old = 0x7E0_u32.to_be_bytes().to_vec();
    expected_old.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_old,
        "the first send must resolve against the OLD Active table (0x7E0), not the staged \
         Working table (0x7DF)"
    );

    // Promote: Working (0x7DF) -> Active.
    promote_via_update_param(&mut client, cll_handle).await;

    // A NEW send now resolves against the newly-promoted Active table.
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;

    let mut expected_new = 0x7DF_u32.to_be_bytes().to_vec();
    expected_new.extend_from_slice(&payload);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_new,
        "the second send must resolve against the newly-promoted Active table (0x7DF)"
    );

    server.shutdown().await;
}

/// `temp_param_update=1` does not change which UniqueRespIdTable a COP
/// reads: it is always the call-time ACTIVE table, never a staged Working
/// table -- unlike ComParamSet, where `temp_param_update` borrows Working
/// for the duration of the call (ADR-067 claim G: the table was never part
/// of that split and still is not here).
#[tokio::test]
#[serial]
async fn temp_param_update_sendrecv_still_uses_active_table_not_staged_working() {
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
        vec![ecu_entry(1, vec![unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0)])],
    )
    .await;

    // Stage a different table in Working only -- never promoted.
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(1, vec![unum32_param(CP_CAN_PHYS_REQ_ID, 0x7DF)])],
    )
    .await;

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

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut expected = 0x7E0_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&payload);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected,
        "temp_param_update=1 must not change which table is read -- still the call-time Active \
         table (0x7E0), not the staged Working table (0x7DF)"
    );

    server.shutdown().await;
}

/// A `temp_param_update=1` COP's completion writes Working back from Active
/// for the ComParamSet (ADR-067 claim D) -- but must NOT reset or otherwise
/// touch the UniqueRespIdTable: a staged Working table survives a temp COP,
/// and a following `CoptUpdateparam` still promotes it.
#[tokio::test]
#[serial]
async fn temp_cop_completion_does_not_wipe_a_staged_working_table() {
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
        vec![ecu_entry(1, vec![unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0)])],
    )
    .await;

    // Stage a different table in Working only.
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(1, vec![unum32_param(CP_CAN_PHYS_REQ_ID, 0x7DF)])],
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // A plain temp_param_update=1 COP -- its own call-time Active table
    // snapshot is the OLD one (0x7E0); this is incidental here, the test is
    // about what happens to the staged Working table AFTER this COP
    // completes, not what this COP itself transmits.
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
    wait_for_cop_finished(&mut events).await;

    // The staged Working table (0x7DF) must still be there -- untouched by
    // the temp COP's Working := Active writeback, which only applies to the
    // ComParamSet.
    let table = get_unique_resp_table(&mut client, cll_handle).await;
    assert_eq!(table[0].unique_resp_identifier, 1);
    let phys_req_id = table[0]
        .params
        .iter()
        .find_map(|p| match (p.id.as_ref(), &p.param_data) {
            (
                Some(vci_service_interface::param_item::Id::ParamId(id)),
                Some(vci_service_interface::param_item::ParamData::Unum32(v)),
            ) if *id == CP_CAN_PHYS_REQ_ID => Some(*v),
            _ => None,
        })
        .expect("CP_CanPhysReqId should be present");
    assert_eq!(
        phys_req_id, 0x7DF,
        "a staged Working table must survive a temp COP's completion writeback"
    );

    // A following CoptUpdateparam still promotes the staged table.
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
    wait_for_cop_finished(&mut events).await;

    let payload = vec![0x02, 0x10, 0x03];
    send_data(&mut client, cll_handle, payload.clone(), vec![]).await;
    let mut expected = 0x7DF_u32.to_be_bytes().to_vec();
    expected.extend_from_slice(&payload);
    assert_eq!(
        server.backdoor.written_data(
            MOCK_CHANNEL_ID,
            server.backdoor.written_count(MOCK_CHANNEL_ID) - 1
        ),
        expected,
        "after CoptUpdateparam, a plain send must use the newly-promoted table (0x7DF)"
    );

    drop(events);
    server.shutdown().await;
}

/// `CoptRestoreParam` copies Active back into Working for the
/// UniqueRespIdTable too (ADR-068), with no hardware filter I/O -- Active
/// itself, and therefore the hardware, is unchanged.
#[tokio::test]
#[serial]
async fn restore_param_copies_active_table_into_working_with_no_filter_io() {
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
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    let original_pattern = server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0);

    // Stage a different table in Working only -- Active (and hardware) is untouched.
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            2,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7EF),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7DF),
            ],
        )],
    )
    .await;
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "Set alone must not touch filters"
    );

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptRestoreParam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptRestoreParam) should succeed");
    wait_for_cop_finished(&mut events).await;

    // Working now mirrors Active again (entry 1, 0x7E0/0x7E8) -- the staged
    // entry 2 is gone from Working.
    let table = get_unique_resp_table(&mut client, cll_handle).await;
    assert_eq!(table.len(), 1);
    assert_eq!(
        table[0].unique_resp_identifier, 1,
        "RestoreParam must copy Active back into Working"
    );

    // No filter I/O happened: same count, same filter (Active never changed).
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        original_pattern,
        "RestoreParam must not touch hardware filters"
    );

    drop(events);
    server.shutdown().await;
}

/// A `CoptUpdateparam` whose UniqueRespIdTable snapshot is unchanged from
/// the current Active table (only a plain ComParam value changed) does NO
/// `FLOW_CONTROL_FILTER` stop/start -- the promotion helper's diff gate.
#[tokio::test]
#[serial]
async fn updateparam_with_unchanged_table_does_no_filter_churn() {
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
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    let original_pattern = server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0);
    let original_flow_control = server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 0);
    // Cumulative `PassThruStartMsgFilter`/`PassThruStopMsgFilter` call counts
    // so far (one `PassThruStartMsgFilter` from the table promotion above).
    // These are asserted unchanged below -- unlike `filter_count`/pattern
    // equality alone, which cannot distinguish "no filter I/O happened" from
    // "an identical filter was stopped and reinstalled" (same net state, two
    // extra hardware calls).
    let start_filter_calls_before = server.backdoor.start_filter_count();
    let stop_filter_calls_before = server.backdoor.stop_filter_count();

    // Change ONLY a plain ComParam (LOOPBACK, supported for every protocol,
    // ADR-028) -- the UniqueRespIdTable's Working set is left exactly as it
    // was (still equal to the current Active table).
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::LOOPBACK, 1).await;

    promote_via_update_param(&mut client, cll_handle).await;

    // The ComParam side did get promoted...
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::LOOPBACK),
        1
    );
    // ...but the table is unchanged, so no filter I/O happened at all: the
    // cumulative start/stop call counts are unchanged (not just the net
    // filter_count/pattern, which a stop-then-reinstall of an identical
    // filter would also leave unchanged).
    assert_eq!(
        server.backdoor.start_filter_count(),
        start_filter_calls_before,
        "diff-gated CoptUpdateparam must not call PassThruStartMsgFilter"
    );
    assert_eq!(
        server.backdoor.stop_filter_count(),
        stop_filter_calls_before,
        "diff-gated CoptUpdateparam must not call PassThruStopMsgFilter"
    );
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 1);
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        original_pattern
    );
    assert_eq!(
        server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 0),
        original_flow_control
    );

    server.shutdown().await;
}

/// A2-14: `SetUniqueRespIdTable` rejecting a ComParam that is not
/// `PDU_PC_UNIQUE_ID` class for the CLL's protocol (ISO 22900-2 §9.3.3.6)
/// must carry a rich `ErrorDetail` -- `PDU_ERR_COMPARAM_NOT_SUPPORTED`, per
/// this crate's post-ADR-105 convention -- not a bare `Status::invalid_argument`.
#[tokio::test]
#[serial]
async fn set_unique_resp_id_table_rejects_non_unique_id_class_comparam_with_error_detail() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // CP_Baudrate (j2534_0404::DATA_RATE) is PDU_PC_BUSTYPE class for CAN
    // family protocols, not PDU_PC_UNIQUE_ID -- UniqueRespIdTable must reject
    // it.
    let status = client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![ecu_entry(
                    1,
                    vec![unum32_param(j2534_0404::DATA_RATE, 500_000)],
                )],
            }),
        })
        .await
        .expect_err("a non-PDU_PC_UNIQUE_ID-class ComParam should be rejected");

    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrComparamNotSupported as i32,
        "ISO 22900-2 §9.3.3.6 non-UNIQUE_ID-class rejection must report \
         PDU_ERR_COMPARAM_NOT_SUPPORTED"
    );

    server.shutdown().await;
}

/// ADR-202: ISO 22900-2:2022 Table B.11 classifies `CP_EcuRespSourceAddress`/
/// `CP_FuncRespFormatPriorityType`/`CP_FuncRespTargetAddr`/
/// `CP_PhysRespFormatPriorityType` as `PDU_PC_UNIQUE_ID` class for SAE J1850
/// VPW/PWM, mirroring the KWP family. Before this fix, `J1850_UNIQUE_ID_UNUM32`
/// listed only `CP_MidRespId`, so `GetUniqueRespIdTable`'s no-table template
/// offered J1850 clients just 1 fillable param, and `SetUniqueRespIdTable`
/// rejected every entry keyed by `CP_EcuRespSourceAddress` with
/// `PDU_ERR_COMPARAM_NOT_SUPPORTED`. Exercised on J1850VPW (the two J1850
/// protocol ids share one `unique_id_params` branch, so either suffices).
#[tokio::test]
#[serial]
async fn j1850_unique_resp_id_table_accepts_the_full_response_addressing_set() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850VPW,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    // No table configured yet: the template must now offer all 5 unum32
    // PDU_PC_UNIQUE_ID params (previously just CP_MidRespId).
    let template = get_unique_resp_table(&mut client, cll_handle).await;
    assert_eq!(
        template.len(),
        1,
        "template mode returns one PDU_ID_UNDEF entry"
    );
    assert_eq!(
        template[0].params.len(),
        5,
        "J1850's UNIQUE_ID template should offer 5 unum32 params, not 1"
    );
    const CP_ECU_RESP_SOURCE_ADDR: u32 = 0x8070;
    const CP_FUNC_RESP_FORMAT_PRIORITY: u32 = 0x8073;
    const CP_FUNC_RESP_TARGET_ADDR: u32 = 0x8074;
    const CP_PHYS_RESP_FORMAT_PRIORITY: u32 = 0x8077;
    const CP_MID_RESP_ID: u32 = 0x8085;
    for expected_id in [
        CP_ECU_RESP_SOURCE_ADDR,
        CP_FUNC_RESP_FORMAT_PRIORITY,
        CP_FUNC_RESP_TARGET_ADDR,
        CP_PHYS_RESP_FORMAT_PRIORITY,
        CP_MID_RESP_ID,
    ] {
        assert!(
            template[0]
                .params
                .iter()
                .any(|p| p.id == Some(vci_service_interface::param_item::Id::ParamId(expected_id))),
            "template should include param {expected_id:#x}"
        );
    }

    // Setting an entry keyed by CP_EcuRespSourceAddress (previously rejected
    // with PDU_ERR_COMPARAM_NOT_SUPPORTED) now succeeds, along with the other
    // 3 newly-reclassified params plus the retained CP_MidRespId surplus
    // entry -- all 5 UNIQUE_ID params staged together in one entry, not just
    // the 4 new ones (list-order independence: CP_MidRespId is the list's
    // trailing/original entry, the other 4 are newly prepended).
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_ECU_RESP_SOURCE_ADDR, 0x10),
                unum32_param(CP_FUNC_RESP_FORMAT_PRIORITY, 0x68),
                unum32_param(CP_FUNC_RESP_TARGET_ADDR, 0xFE),
                unum32_param(CP_PHYS_RESP_FORMAT_PRIORITY, 0x68),
                unum32_param(CP_MID_RESP_ID, 0x01),
            ],
        )],
    )
    .await;

    let table = get_unique_resp_table(&mut client, cll_handle).await;
    assert_eq!(table.len(), 1);
    assert_eq!(table[0].unique_resp_identifier, 1);
    assert_eq!(table[0].params.len(), 5);

    server.shutdown().await;
}

/// ADR-202 (`edge-case-hunter` finding, this PR's own close-out pass): a
/// J1850 entry mixing a newly-valid `PDU_PC_UNIQUE_ID` param
/// (`CP_EcuRespSourceAddress`) with a param that is still NOT
/// `PDU_PC_UNIQUE_ID`-class for J1850 (`CP_CanRespUSDTId`, a CAN-only
/// param) must still be rejected -- proving the per-entry validation loop
/// (`rpc_misc.rs::rpc_set_unique_resp_id_table`) rejects per-item, not
/// wholesale once any one param in the entry validates.
#[tokio::test]
#[serial]
async fn j1850_unique_resp_id_table_rejects_a_can_only_param_mixed_with_a_valid_one() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::J1850VPW,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    const CP_ECU_RESP_SOURCE_ADDR: u32 = 0x8070;
    const CP_CAN_RESP_USDT_ID: u32 = 0x8065;

    let status = client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![ecu_entry(
                    1,
                    vec![
                        unum32_param(CP_ECU_RESP_SOURCE_ADDR, 0x10),
                        unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                    ],
                )],
            }),
        })
        .await
        .expect_err("a CAN-only param mixed into a J1850 entry should still be rejected");

    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrComparamNotSupported as i32
    );

    server.shutdown().await;
}

/// ADR-222 (motivating scenario): a raw-CAN channel (`software-isotp` mode's
/// own primary channel, ADR-065) with a SINGLE table configuring the same
/// numeric `CP_CanRespUUDTId` at two different widths -- entry 1 at the
/// 11-bit default, entry 2 explicitly 29-bit (`CP_CanRespUUDTFormat` bit 1).
/// An 11-bit-tagged frame must resolve to entry 1; a 29-bit-tagged frame
/// (numerically identical) must resolve to entry 2, not the first/only
/// pre-ADR-222 numeric match.
#[tokio::test]
#[serial]
async fn contended_uudt_id_disambiguates_by_can_id_width_on_a_single_table() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
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
        vec![
            // Entry 1: 11-bit (no Format ComParam -- defaults to 11-bit).
            // Also carries CP_CanPhysReqId -- `resolve_can_addressing`
            // (`tx_header.rs`) requires the table's first entry to set it
            // before any CoptSendrecv (even `arm_receive_only_monitor`'s
            // zero-send-cycle one) can resolve TX addressing.
            ecu_entry(
                1,
                vec![
                    unum32_param(CP_CAN_RESP_UUDT_ID, 0x123),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            // Entry 2: same numeric id, explicitly 29-bit.
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_UUDT_ID, 0x123),
                    unum32_param(CP_CAN_RESP_UUDT_FORMAT, 0x02),
                ],
            ),
        ],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload_11bit = vec![0xAA];
    let frame_11bit = can_frame(0x123, &payload_11bit);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_11bit, j2534_0404::CAN);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload_11bit);
    assert_eq!(
        result.unique_resp_identifier, 1,
        "an 11-bit-tagged frame must resolve to the 11-bit entry"
    );

    let payload_29bit = vec![0xBB];
    let frame_29bit = can_frame(0x123, &payload_29bit);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame_29bit,
        j2534_0404::CAN,
        j2534_0404::CAN_29BIT_ID_STATUS,
    );
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload_29bit);
    assert_eq!(
        result.unique_resp_identifier, 2,
        "a 29-bit-tagged frame (numerically identical to entry 1's id) must resolve to the \
         29-bit entry, not the first/only pre-ADR-222 numeric match"
    );

    server.shutdown().await;
}

/// ADR-222: a CROSS-FIELD collision -- entry 1's `CP_CanRespUSDTId` and
/// entry 2's `CP_CanRespUUDTId` share the same numeric value at different
/// widths, rather than the same field on both entries. `UniqueRespIdKey::
/// matched` checks the USDT tier before the UUDT tier, so a contention map
/// that tracked USDT and UUDT ids as independent id spaces (an earlier,
/// buggy version of this fix -- edge-case-hunter finding, this PR) would
/// leave entry 1's `usdt_width_gate` at `Any` (its OWN field never saw a
/// second width) and a 29-bit frame would vacuously match entry 1's USDT
/// tier before entry 2's UUDT tier is ever reached.
#[tokio::test]
#[serial]
async fn contended_id_disambiguates_across_usdt_and_uudt_fields_on_different_entries() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
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
        vec![
            // Entry 1: USDT id, 11-bit (no Format ComParam -- defaults to
            // 11-bit). Also carries CP_CanPhysReqId for the same
            // `resolve_can_addressing` reason as the single-table test above.
            ecu_entry(
                1,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x123),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            // Entry 2: UUDT id, same numeric value, explicitly 29-bit.
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_UUDT_ID, 0x123),
                    unum32_param(CP_CAN_RESP_UUDT_FORMAT, 0x02),
                ],
            ),
        ],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload_11bit = vec![0xAA];
    let frame_11bit = can_frame(0x123, &payload_11bit);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_11bit, j2534_0404::CAN);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload_11bit);
    assert_eq!(
        result.unique_resp_identifier, 1,
        "an 11-bit-tagged frame must resolve to entry 1's USDT tier"
    );

    let payload_29bit = vec![0xBB];
    let frame_29bit = can_frame(0x123, &payload_29bit);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame_29bit,
        j2534_0404::CAN,
        j2534_0404::CAN_29BIT_ID_STATUS,
    );
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload_29bit);
    assert_eq!(
        result.unique_resp_identifier, 2,
        "a 29-bit-tagged frame must resolve to entry 2's UUDT tier, not vacuously match \
         entry 1's USDT tier just because entry 1's OWN field never saw a second width"
    );

    server.shutdown().await;
}

/// ADR-222 (Codex review finding, PR #141): a dual-channel CLL's own USDT id
/// (received ONLY on its primary channel) and a SEPARATE entry's UUDT id
/// (received ONLY on its own ADR-046 companion channel, a DIFFERENT physical
/// channel) happen to share the same numeric value at different widths --
/// these two ids are never actually contended in received-frame terms, since
/// a frame at that numeric value on the primary channel can only ever be the
/// USDT (29-bit-configured) one, and a frame at that value on the companion
/// channel can only ever be the UUDT (11-bit-configured) one. Deliberately
/// TWO SEPARATE entries (URID 9 vs. URID 10), not one entry configuring both
/// fields: with one entry, `UniqueRespIdKey::matched`'s own USDT-then-UUDT
/// fallback would still resolve to the SAME `unique_resp_identifier`
/// regardless of which tier matched, masking this bug from an
/// identifier-only assertion -- an earlier draft of this test made exactly
/// that mistake. With two entries, a false-contention bug does not drop the
/// frame outright (the sibling entry's own UUDT tier, numerically equal,
/// still matches it) -- it MISATTRIBUTES it to the wrong entry, which this
/// test can actually observe as a wrong `unique_resp_identifier`.
///
/// An earlier, buggy version of the contention-map collection unconditionally
/// counted BOTH `CP_CanRespUSDTId` and `CP_CanRespUUDTId` from EVERY entry of
/// EVERY CLL touching either `channel_id` or `uudt_channel_id`, regardless of
/// which physical channel each field's traffic is actually carried on --
/// cross-contributing the companion-only entry's UUDT width into the primary
/// channel's own (otherwise-uncontended) USDT id, falsely marking it
/// contended and turning its gate from `Any` into a strict `Bits29`. This
/// test's frame is injected via plain `inject_rx` (mock default `RxStatus ==
/// 0`, i.e. `frame_is_29bit == false`) even though the USDT entry is
/// configured 29-bit -- reproducing an adapter that does not honestly report
/// `RxStatus` bit 8. Under the bug, entry 9's `Bits29` gate rejects the
/// `false`-tagged frame, and `matched`'s `find_map` falls through to entry
/// 10's OWN `Bits11` UUDT tier (contended, configured 11-bit, accepts
/// `false`) -- delivering the frame under URID 10 instead of the correct 9.
/// Correctly scoped, the primary channel's own USDT id is uncontended (only
/// one width is ever actually observed ON THAT CHANNEL), so entry 9's gate
/// stays `Any`, delivers correctly under URID 9, and entry 10 (a genuinely
/// different, companion-only entry) is never reached at all.
#[tokio::test]
#[serial]
async fn dual_channel_same_numeric_id_across_primary_and_companion_is_not_falsely_contended() {
    let server = TestServer::start_with_can_mode(Some("dual-channel")).await;
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
        vec![
            // Entry 9: USDT only -- received on the primary ISO15765
            // channel, explicitly 29-bit.
            ecu_entry(
                9,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x321),
                    unum32_param(CP_CAN_RESP_USDT_FORMAT, 0x02),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            // Entry 10: UUDT only, same numeric value -- received only on
            // the separate ADR-046 companion channel, 11-bit (no Format
            // ComParam, defaults to 11-bit).
            ecu_entry(10, vec![unum32_param(CP_CAN_RESP_UUDT_ID, 0x321)]),
        ],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    // A 29-bit USDT frame on the PRIMARY channel, injected with the mock's
    // default RxStatus == 0 (frame_is_29bit == false) -- an adapter that
    // never sets CAN_29BIT_ID_STATUS. Must still deliver under entry 9: this
    // id is uncontended on the primary channel (entry 10's own UUDT
    // observation, carried only on the companion channel, must not leak
    // into this channel's contention map).
    let payload = vec![0xAA];
    let frame = can_frame(0x321, &payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x321_u32.to_be_bytes(), &[], &payload);
    assert_eq!(
        result.unique_resp_identifier, 9,
        "the primary channel's own USDT entry must not be falsely marked contended, and \
         must not be shadowed by entry 10's own companion-only UUDT id at the same numeric \
         value"
    );

    server.shutdown().await;
}

/// ADR-222: the SAME numeric `CP_CanRespUUDTId` collision as above, but
/// contended ACROSS two CLLs (X, Y) sharing one raw-CAN `(CAN, baud)`
/// `CAN_ID_BOTH` channel (ADR-065's own motivating scenario) rather than
/// within one CLL's own table -- CLL X configures the id at 11-bit, CLL Y at
/// 29-bit. Each frame's own width must route it to the correct CLL, never
/// the sibling.
#[tokio::test]
#[serial]
async fn contended_uudt_id_disambiguates_by_can_id_width_across_sibling_clls_sharing_a_channel() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
    let mut client = server.client().await;

    let cll_x = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_y = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_x and cll_y should share one physical raw-CAN channel"
    );

    // Each entry also carries its own CP_CanPhysReqId -- see the
    // single-table test's own comment on why `arm_receive_only_monitor`
    // needs it.
    set_unique_resp_table_and_promote(
        &mut client,
        cll_x,
        vec![ecu_entry(
            1,
            vec![
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x123),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_y,
        vec![ecu_entry(
            2,
            vec![
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x123),
                unum32_param(CP_CAN_RESP_UUDT_FORMAT, 0x02),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7F0),
            ],
        )],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_x).await;
    arm_receive_only_monitor(&mut client, cll_y).await;

    let payload_11bit = vec![0xAA];
    let frame_11bit = can_frame(0x123, &payload_11bit);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_11bit, j2534_0404::CAN);
    let result = wait_for_result_data(&mut client, cll_x).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload_11bit);
    assert_eq!(result.unique_resp_identifier, 1);
    assert_no_result_data(
        &mut client,
        cll_y,
        "an 11-bit-tagged frame must not reach the 29-bit-configured sibling CLL",
    )
    .await;

    let payload_29bit = vec![0xBB];
    let frame_29bit = can_frame(0x123, &payload_29bit);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame_29bit,
        j2534_0404::CAN,
        j2534_0404::CAN_29BIT_ID_STATUS,
    );
    let result = wait_for_result_data(&mut client, cll_y).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload_29bit);
    assert_eq!(result.unique_resp_identifier, 2);
    assert_no_result_data(
        &mut client,
        cll_x,
        "a 29-bit-tagged frame must not reach the 11-bit-configured sibling CLL",
    )
    .await;

    server.shutdown().await;
}

/// ADR-222: the same disambiguation on a NATIVE ISO15765 hardware channel
/// (not raw/software-isotp) -- two entries sharing a numeric
/// `CP_CanRespUSDTId` at different widths, each getting its own
/// point-to-point `FLOW_CONTROL_FILTER` (ADR-048's `install_point_to_point_
/// fc_filters`, one filter per table entry, the second situation ADR-222's
/// own Context section names).
#[tokio::test]
#[serial]
async fn contended_usdt_id_disambiguates_by_can_id_width_on_native_iso15765() {
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
        vec![
            // Entry 1: 11-bit (no Format ComParam -- defaults to 11-bit, FC
            // enabled).
            ecu_entry(
                1,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x123),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            // Entry 2: same numeric id, explicitly 29-bit with FC enabled
            // (bit 1 + bit 0).
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x123),
                    unum32_param(CP_CAN_RESP_USDT_FORMAT, 0x03),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7F0),
                ],
            ),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        2,
        "each entry gets its own point-to-point FLOW_CONTROL_FILTER (ADR-048)"
    );

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload_11bit = vec![0x62, 0xF1, 0x90, 0xAA];
    let frame_11bit = can_frame(0x123, &payload_11bit);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_11bit, j2534_0404::ISO15765);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload_11bit);
    assert_eq!(
        result.unique_resp_identifier, 1,
        "an 11-bit-tagged frame must resolve to the 11-bit entry"
    );

    let payload_29bit = vec![0x62, 0xF1, 0x90, 0xBB];
    let frame_29bit = can_frame(0x123, &payload_29bit);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame_29bit,
        j2534_0404::ISO15765,
        j2534_0404::CAN_29BIT_ID_STATUS,
    );
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload_29bit);
    assert_eq!(
        result.unique_resp_identifier, 2,
        "a 29-bit-tagged frame must resolve to the 29-bit entry, not the first/only \
         pre-ADR-222 numeric match"
    );

    server.shutdown().await;
}

/// ADR-222 regression-safety (the "provable no-op" guarantee): an
/// UNCONTENDED table -- a single entry configuring a 29-bit id with no
/// colliding 11-bit entry anywhere on the channel -- must still deliver
/// normally even when the injected frame's `RxStatus` is `0` (the mock's
/// default, i.e. bit 8/`CAN_29BIT_ID_STATUS` NOT set, as every pre-ADR-222
/// `grpc_mock` fixture already injects). Proves an uncontended id's gate is
/// `Any` and does not newly require a device to honestly report the width
/// bit.
#[tokio::test]
#[serial]
async fn uncontended_29bit_id_still_delivers_with_rx_status_zero() {
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
                unum32_param(CP_CAN_RESP_USDT_ID, 0x18DA_F110),
                unum32_param(CP_CAN_RESP_USDT_FORMAT, 0x02),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x62, 0xF1, 0x90, 0xAA];
    let frame = can_frame(0x18DA_F110, &payload);
    // Plain `inject_rx` (RxStatus == 0, no CAN_29BIT_ID_STATUS bit) -- this
    // id is uncontended (no sibling 11-bit entry exists anywhere on the
    // channel), so its gate is `Any` and must accept regardless.
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::ISO15765);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x18DA_F110_u32.to_be_bytes(), &[], &payload);
    assert_eq!(result.unique_resp_identifier, 1);

    server.shutdown().await;
}

/// ADR-222 (Codex review finding, PR #141): a software-ISO-TP reassembly for
/// an UNCONTENDED USDT id must complete even when a device's own `RxStatus`
/// bit 8 (`CAN_29BIT_ID`) reporting is INCONSISTENT across one segmented
/// transfer's own frames -- the FirstFrame tagged 29-bit, its completing
/// ConsecutiveFrame tagged plain (`RxStatus == 0`). Since this id is
/// uncontended (only one width is ever configured for it anywhere on the
/// channel), its gate is `Any` and routing accepts BOTH frames regardless of
/// their own reported width -- but an earlier, buggy version of the
/// reassembly-map key derived the key's width component directly from each
/// frame's own raw `frame_is_29bit` bit (`(can_id, frame.frame_is_29bit)`),
/// rather than from the matched entry's canonical `CanIdWidthGate`. That
/// stored the FirstFrame's state under `(can_id, true)` and then looked up
/// the ConsecutiveFrame under `(can_id, false)` -- a miss, silently
/// dropping the response as a "stray CF with no reassembly in progress"
/// even though `Any`'s own routing tolerance says these two frames belong
/// to the same transfer. This is a regression from the pre-ADR-222
/// CAN-ID-only key, which never had this sensitivity at all.
#[tokio::test]
#[serial]
async fn uncontended_id_reassembly_tolerates_inconsistent_rx_status_bit8_across_one_transfer() {
    let server = TestServer::start_with_can_mode(Some("software-isotp")).await;
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
            7,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let response_payload: Vec<u8> = (0x60..0x60 + 10).collect();

    // FirstFrame tagged 29-bit.
    let mut ff = 0x7E8_u32.to_be_bytes().to_vec();
    ff.extend_from_slice(&[0x10, 0x0A]);
    ff.extend_from_slice(&response_payload[..6]);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &ff,
        j2534_0404::CAN,
        j2534_0404::CAN_29BIT_ID_STATUS,
    );
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // Completing ConsecutiveFrame tagged plain (RxStatus == 0) -- the same
    // device, mid-transfer, no longer reporting bit 8. Must still complete
    // the SAME reassembly this id's uncontended `Any` gate already accepted
    // both frames under.
    let mut cf = 0x7E8_u32.to_be_bytes().to_vec();
    cf.push(0x21);
    cf.extend_from_slice(&response_payload[6..10]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &cf, j2534_0404::CAN);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x7E8_u32.to_be_bytes(), &[], &response_payload);
    assert_eq!(result.unique_resp_identifier, 7);

    server.shutdown().await;
}

/// ADR-222 round 4 (design-advisor audit, PR #141): a native-mixed entry
/// configuring `CP_CanRespUSDTId` and `CP_CanRespUUDTId` at the SAME numeric
/// id but DIFFERENT widths must deliver BOTH interpretations even though
/// neither injected frame's own `RxStatus` bit 8 matches its own configured
/// width -- proving the domain-scoped contention model (a native-mixed
/// entry's USDT id is only ever contended against OTHER ids compared by the
/// ISO-tagged frame population, and its UUDT id only ever against ids
/// compared by the raw-CAN-tagged population) rather than the pre-round-4
/// single combined map, which would have falsely contended THIS SAME
/// entry's own two fields against each other (both configure numeric id
/// 0x321) and turned each field's gate into a strict width check that a
/// `RxStatus == 0` frame fails for the 29-bit-configured UUDT field.
/// `native-mixed-all-frames` (not plain `native-mixed`) is used so
/// connecting a table whose USDT and UUDT match keys share one numeric id is
/// accepted rather than rejected by ADR-162's collision check (ADR-217
/// Decision item 8) -- mirrors `mixed_format_can.rs`'s own
/// `native_mixed_all_frames_mode_delivers_both_usdt_and_uudt_interpretations`.
#[tokio::test]
#[serial]
async fn native_mixed_all_frames_same_entry_usdt_uudt_different_widths_both_deliver_with_rx_status_zero()
 {
    let server = TestServer::start_with_can_mode(Some("native-mixed-all-frames")).await;
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
            3,
            vec![
                // USDT: 11-bit (no Format ComParam -- defaults to 11-bit).
                unum32_param(CP_CAN_RESP_USDT_ID, 0x321),
                // UUDT: same numeric id, explicitly 29-bit.
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x321),
                unum32_param(CP_CAN_RESP_UUDT_FORMAT, 0x02),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    arm_receive_only_monitor(&mut client, cll_handle).await;

    // CAN-tagged (raw) delivery via the UUDT/PASS_FILTER path -- plain
    // `inject_rx` (RxStatus == 0, i.e. frame_is_29bit == false) despite this
    // entry's own CP_CanRespUUDTId being configured 29-bit. Under the
    // pre-round-4 combined map, id 0x321 would be falsely contended (widths
    // {false, true} from combining the USDT and UUDT fields of this SAME
    // entry), giving the UUDT field a Bits29 gate that rejects this frame.
    let uudt_payload = vec![0xAA];
    let uudt_frame = can_frame(0x321, &uudt_payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &uudt_frame, j2534_0404::CAN);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x321_u32.to_be_bytes(), &[], &uudt_payload);
    assert_eq!(
        result.unique_resp_identifier, 3,
        "the CAN-tagged (UUDT) interpretation must deliver despite RxStatus == 0 not matching \
         this field's own configured 29-bit width -- native-mixed entries route USDT and UUDT \
         through disjoint frame populations, not width"
    );

    // ISO15765-tagged delivery via the USDT path -- same RxStatus == 0.
    let usdt_payload = vec![0x62, 0xF1, 0x90, 0xBB];
    let usdt_frame = can_frame(0x321, &usdt_payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &usdt_frame, j2534_0404::ISO15765);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x321_u32.to_be_bytes(), &[], &usdt_payload);
    assert_eq!(
        result.unique_resp_identifier, 3,
        "the ISO15765-tagged (USDT) interpretation must also deliver"
    );

    server.shutdown().await;
}

/// ADR-222 round 4 (design-advisor audit, PR #141): on a dual-channel-mode
/// (ADR-046) primary channel, the UUDT tier must be disabled entirely for
/// an ISO-tagged frame -- regardless of `UniqueRespIdTable` entry iteration
/// order -- since no `PASS_FILTER`/`FLOW_CONTROL_FILTER` for a UUDT id is
/// ever installed on the primary channel at all (UUDT is only ever received
/// via the separate companion channel). Entry 10 (UUDT only) is listed
/// FIRST and entry 9 (USDT only) SECOND, both at the same numeric id 0x321;
/// without the `uudt_on_companion` routing gate, `matched`'s `find_map`
/// would reach entry 10's own (now-disabled) UUDT tier before entry 9's
/// USDT tier, misdelivering under URID 10 instead of the correct URID 9.
#[tokio::test]
#[serial]
async fn dual_channel_primary_uudt_tier_disabled_regardless_of_table_iteration_order() {
    let server = TestServer::start_with_can_mode(Some("dual-channel")).await;
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
        vec![
            // Entry 10: UUDT only, listed FIRST. Also carries
            // CP_CanPhysReqId: `resolve_can_addressing` (`tx_header.rs`)
            // resolves TX addressing from `entries.first()` specifically
            // (the table's literal first row, not "any entry with a
            // CP_CanPhysReqId"), and does so even for
            // `arm_receive_only_monitor`'s zero-send-cycle CoptSendrecv --
            // harmless here since `CP_CanPhysReqId` plays no part in
            // `WidthDomain`/contention resolution, only USDT/UUDT id +
            // Format do.
            ecu_entry(
                10,
                vec![
                    unum32_param(CP_CAN_RESP_UUDT_ID, 0x321),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            // Entry 9: USDT only, listed SECOND.
            ecu_entry(
                9,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x321),
                    unum32_param(CP_CAN_RESP_USDT_FORMAT, 0x02),
                ],
            ),
        ],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    // An ISO15765-tagged frame at 0x321 on the PRIMARY channel, RxStatus ==
    // 0 (the mock's default).
    let payload = vec![0x62, 0xF1, 0x90, 0xAA];
    let frame = can_frame(0x321, &payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::ISO15765);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x321_u32.to_be_bytes(), &[], &payload);
    assert_eq!(
        result.unique_resp_identifier, 9,
        "the ISO-tagged frame must deliver under the USDT entry (9), not entry 10's own dead \
         UUDT tier on the dual-channel primary -- regardless of table iteration order"
    );

    server.shutdown().await;
}

/// ADR-222 round 4 (design-advisor audit, PR #141): a non-regression guard
/// -- two entries both configuring `CP_CanRespUUDTId` at the same numeric
/// value but different widths on a native-mixed CLL are both in the SAME
/// `can` domain, so they must still disambiguate by width, exactly like the
/// pre-round-4 single-combined-map model already proved
/// (`contended_uudt_id_disambiguates_by_can_id_width_on_a_single_table`,
/// same shape, but under `native-mixed` mode instead of `software-isotp`).
/// Since both fields land in the SAME domain regardless of the round-4
/// split, this scenario would already pass under the pre-round-4 code too
/// -- it is not itself a fail-before/pass-after proof that the `iso`/`can`
/// split fixed anything (that proof is
/// `native_mixed_all_frames_same_entry_usdt_uudt_different_widths_both_deliver_with_rx_status_zero`,
/// which cross-field/cross-population `Any`-gates without this split).
#[tokio::test]
#[serial]
async fn native_mixed_mode_uudt_uudt_collision_still_disambiguates_by_width() {
    let server = TestServer::start_with_can_mode(Some("native-mixed")).await;
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
        vec![
            // Entry 1: 11-bit (no Format ComParam -- defaults to 11-bit).
            // Also carries CP_CanPhysReqId -- `resolve_can_addressing`
            // requires the table's first entry to set it before any
            // CoptSendrecv (even `arm_receive_only_monitor`'s zero-send-cycle
            // one) can resolve TX addressing.
            ecu_entry(
                1,
                vec![
                    unum32_param(CP_CAN_RESP_UUDT_ID, 0x123),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            // Entry 2: same numeric id, explicitly 29-bit, and (ADR-222
            // round 6) deliberately with NO `CP_CanPhysReqId` -- under
            // native-mixed mode a UUDT `PASS_FILTER` is installed
            // unconditionally regardless of `CP_CanPhysReqId`
            // (`point_to_point_filter_eligibility`'s `native_mixed ||
            // has_req` UUDT clause), so entry 2 is still fully eligible and
            // must still contend/disambiguate -- proving the round-6
            // eligibility gate does not over-relax a case that doesn't need
            // it.
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_UUDT_ID, 0x123),
                    unum32_param(CP_CAN_RESP_UUDT_FORMAT, 0x02),
                ],
            ),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        2,
        "both entries are eligible under native-mixed mode (unconditional PASS_FILTER), \
         regardless of entry 2 having no CP_CanPhysReqId (ADR-222 round 6)"
    );

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload_11bit = vec![0xAA];
    let frame_11bit = can_frame(0x123, &payload_11bit);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_11bit, j2534_0404::CAN);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload_11bit);
    assert_eq!(
        result.unique_resp_identifier, 1,
        "an 11-bit-tagged frame must resolve to the 11-bit entry"
    );

    let payload_29bit = vec![0xBB];
    let frame_29bit = can_frame(0x123, &payload_29bit);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame_29bit,
        j2534_0404::CAN,
        j2534_0404::CAN_29BIT_ID_STATUS,
    );
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload_29bit);
    assert_eq!(
        result.unique_resp_identifier, 2,
        "a 29-bit-tagged frame (numerically identical to entry 1's id) must resolve to the \
         29-bit entry, not the first/only pre-ADR-222 numeric match -- the domain split must \
         not over-relax a genuine same-domain UUDT/UUDT collision"
    );

    server.shutdown().await;
}

/// ADR-222 round 4 (edge-case-hunter coverage-gap finding, PR #141): the
/// `Iso`-domain symmetric counterpart to
/// `native_mixed_mode_uudt_uudt_collision_still_disambiguates_by_width` --
/// two DIFFERENT native-mixed entries both configuring `CP_CanRespUSDTId`
/// at the same numeric value but different widths must still disambiguate
/// by width, proving the `Iso` domain alone (not just the `Can` domain the
/// existing UUDT/UUDT test covers) correctly detects a genuine same-domain
/// collision rather than over-relaxing it. Unlike the UUDT/UUDT test, this
/// scenario is NOT already correct under the pre-round-4 single-combined-map
/// code by coincidence of both fields landing in one shared space -- a
/// native-mixed entry's own USDT field only ever lands in the `iso` domain,
/// so this specifically exercises `width_domains`' `Iso`-only classification
/// path for TWO DIFFERENT entries both contributing to that same domain.
#[tokio::test]
#[serial]
async fn native_mixed_mode_usdt_usdt_collision_still_disambiguates_by_width() {
    let server = TestServer::start_with_can_mode(Some("native-mixed")).await;
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
        vec![
            // Entry 1: 11-bit (no Format ComParam -- defaults to 11-bit, FC
            // enabled).
            ecu_entry(
                1,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x123),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            // Entry 2: same numeric id, explicitly 29-bit with FC enabled
            // (bit 1 + bit 0) and its own CP_CanPhysReqId -- Codex review
            // finding, this PR: a bare 29-bit-only Format (bit 1, no bit 0)
            // leaves flow control disabled, which `install_point_to_point_
            // fc_filters` (`rpc_link.rs:5093-5130`) treats as "skip this
            // entry" on a real channel -- the claimed 29-bit frame could
            // never actually reach production hardware through this entry,
            // so the test would only pass via the mock backdoor's
            // filter-bypassing `inject_rx`, without exercising the real
            // scenario it claims to. Mirrors `contended_usdt_id_
            // disambiguates_by_can_id_width_on_native_iso15765`'s own
            // entry 2.
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x123),
                    unum32_param(CP_CAN_RESP_USDT_FORMAT, 0x03),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7F0),
                ],
            ),
        ],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload_11bit = vec![0x62, 0xF1, 0x90, 0xAA];
    let frame_11bit = can_frame(0x123, &payload_11bit);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame_11bit, j2534_0404::ISO15765);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload_11bit);
    assert_eq!(
        result.unique_resp_identifier, 1,
        "an 11-bit-tagged ISO15765 frame must resolve to the 11-bit entry"
    );

    let payload_29bit = vec![0x62, 0xF1, 0x90, 0xBB];
    let frame_29bit = can_frame(0x123, &payload_29bit);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &frame_29bit,
        j2534_0404::ISO15765,
        j2534_0404::CAN_29BIT_ID_STATUS,
    );
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload_29bit);
    assert_eq!(
        result.unique_resp_identifier, 2,
        "a 29-bit-tagged ISO15765 frame (numerically identical to entry 1's id) must resolve \
         to the 29-bit entry -- the Iso domain alone must still disambiguate a genuine \
         same-domain USDT/USDT collision"
    );

    server.shutdown().await;
}

/// ADR-222 round 6 (design-advisor audit, PR #141): a `UniqueRespIdTable`
/// entry that would never actually receive a point-to-point
/// `FLOW_CONTROL_FILTER` on real native ISO15765 hardware must not falsely
/// mark a DIFFERENT, properly-filtered sibling entry's own numeric id as
/// contended. Entry A (URID 2, listed FIRST) is fully eligible
/// (flow-control-enabled 29-bit Format, `CP_CanPhysReqId` set) and gets its
/// own `FLOW_CONTROL_FILTER`. Entry B (URID 1) shares the same numeric id,
/// defaults to 11-bit, but has NO `CP_CanPhysReqId` -- `install_point_to_
/// point_fc_filters` (ADR-039) skips it entirely, so on real hardware it
/// could never actually receive a frame of its own. Before this fix, entry
/// B's default-11-bit field still contributed a width observation to the
/// contention map anyway, wrongly marking `0x123` contended and gating
/// entry A to a strict `Bits29` check that a plain, untagged (`RxStatus ==
/// 0`) frame -- exactly what an adapter that omits `RxStatus` bit 8 sends --
/// would fail.
#[tokio::test]
#[serial]
async fn ineligible_sibling_usdt_entry_does_not_falsely_contend_an_eligible_entrys_id() {
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
        vec![
            // Entry A (URID 2), listed FIRST: fully eligible -- flow-control
            // enabled + 29-bit Format, and CP_CanPhysReqId set (also
            // satisfies `resolve_can_addressing`'s requirement that the
            // table's first entry configure CP_CanPhysReqId). Gets a real
            // FLOW_CONTROL_FILTER.
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_USDT_ID, 0x123),
                    unum32_param(CP_CAN_RESP_USDT_FORMAT, 0x03),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            // Entry B (URID 1): same numeric id, defaults to 11-bit
            // (flow-control-enabled by `CanIdFormat::default()`), but has NO
            // CP_CanPhysReqId -- `install_point_to_point_fc_filters` skips
            // it (ADR-039).
            ecu_entry(1, vec![unum32_param(CP_CAN_RESP_USDT_ID, 0x123)]),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "entry B has no CP_CanPhysReqId, so only entry A's FLOW_CONTROL_FILTER is installed"
    );

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x62, 0xF1, 0x90, 0xAA];
    let frame = can_frame(0x123, &payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::ISO15765);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload);
    assert_eq!(
        result.unique_resp_identifier, 2,
        "must deliver under entry A (URID 2) -- entry B's ineligible field must not falsely \
         mark 0x123 contended and gate entry A to a strict width check a plain, untagged \
         frame would fail"
    );

    server.shutdown().await;
}

/// UUDT/UUDT counterpart to
/// `ineligible_sibling_usdt_entry_does_not_falsely_contend_an_eligible_entrys_id`
/// (ADR-222 round 6): default single-channel mode, ADR-041 UUDT
/// `FLOW_CONTROL_FILTER` workaround. Entry A (URID 2, listed FIRST) has
/// `CP_CanPhysReqId` set, so its UUDT field is eligible and gets a real
/// `FLOW_CONTROL_FILTER`. Entry B (URID 1) shares the same numeric id but
/// has no `CP_CanPhysReqId` -- UUDT's own Format flow-control bit is never
/// consulted for eligibility (unlike USDT's), but the `CP_CanPhysReqId`
/// requirement still applies outside native-mixed mode (ADR-041), so entry
/// B's field never gets a filter of its own either.
#[tokio::test]
#[serial]
async fn ineligible_sibling_uudt_entry_does_not_falsely_contend_an_eligible_entrys_id() {
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
        vec![
            // Entry A (URID 2), listed FIRST: 29-bit Format, CP_CanPhysReqId
            // set (also satisfies `resolve_can_addressing`'s requirement
            // that the table's first entry configure CP_CanPhysReqId). Gets
            // a real FLOW_CONTROL_FILTER (ADR-041).
            ecu_entry(
                2,
                vec![
                    unum32_param(CP_CAN_RESP_UUDT_ID, 0x123),
                    unum32_param(CP_CAN_RESP_UUDT_FORMAT, 0x02),
                    unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                ],
            ),
            // Entry B (URID 1): same numeric id, defaults to 11-bit, but has
            // NO CP_CanPhysReqId -- the ADR-041 FLOW_CONTROL_FILTER
            // workaround skips it.
            ecu_entry(1, vec![unum32_param(CP_CAN_RESP_UUDT_ID, 0x123)]),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "entry B has no CP_CanPhysReqId, so only entry A's FLOW_CONTROL_FILTER is installed"
    );

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0xAA];
    let frame = can_frame(0x123, &payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);
    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x123_u32.to_be_bytes(), &[], &payload);
    assert_eq!(
        result.unique_resp_identifier, 2,
        "must deliver under entry A (URID 2) -- entry B's ineligible field must not falsely \
         mark 0x123 contended and gate entry A to a strict width check a plain, untagged \
         frame would fail"
    );

    server.shutdown().await;
}

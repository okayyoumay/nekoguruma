//! ComParam class boundaries and physical-resource locking:
//! `PDU_PC_UNIQUE_ID`-class params are exchanged only through
//! `SetUniqueRespIdTable`, never `SetComParam` (ISO 22900-2 §9.3.3.6,
//! ADR-042), and operations that touch shared physical channel state respect
//! `LOCK_PHYSICAL_COM_PARAMS` held by another CLL (ADR-043/045). `CoptUpdateparam`
//! and `temp_param_update` are the exception: per ISO 22900-2 §9.4.16 d),
//! a `LOCK_PHYSICAL_COM_PARAMS` conflict on a `PDU_PC_BUSTYPE`-class ComParam
//! never rejects the call -- the COP is created, every non-conflicting param
//! still applies, and the conflict surfaces only as a single
//! `PDU_ERR_EVT_RSC_LOCKED` error event, with the COP still reaching
//! `PDU_COPST_FINISHED` (ADR-110, superseding ADR-044's synchronous
//! rejection).

use serial_test::serial;
use vci_service_interface::{
    CancelComPrimitiveRequest, ComLogicalLinkHandle, ComPrimitiveCtrlData, ComPrimitiveHandle,
    ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, DestroyComLogicalLinkRequest,
    DisconnectComLogicalLinkRequest, EcuUniqueRespData, ExpectedResponseData, GetStatusRequest,
    LockResourceRequest, ModuleHandle, ParamItem, PduComPrimitiveStatus, PduError,
    SetComParamRequest, SetUniqueRespIdTableRequest, StartComPrimitiveRequest,
    UniqueRespIdTableItem, UnlockResourceRequest, create_com_logical_link_request,
    error_detail_from_status, event_item, get_status_request, param_item, status_response,
};

use crate::harness::*;

/// Waits for the next `PduCopstFinished` on an already-open event stream
/// (must be subscribed BEFORE the COP being waited on is started) -- mirrors
/// the identical helper duplicated across this test suite's other files
/// (e.g. `unique_resp_id_table_binding.rs`).
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

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)` -- mirrors `pdu_ioctl.rs`'s identical
/// helper (this suite's per-file-duplication convention).
async fn resolve_ioctl_id(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
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

/// Issues a CLL-targeted `IoCtl` with no input/output payload -- mirrors
/// `pdu_ioctl.rs`'s identical helper.
async fn io_ctl_cll(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
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

/// Starts a fire-and-forget `CoptSendrecv` (no response expected) and returns
/// immediately -- `StartComPrimitive` only enqueues the `TxItem`, so this
/// does not wait for the poll task to dispatch it. Mirrors `pdu_ioctl.rs`'s
/// identical helper.
async fn start_send_recv(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
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

/// Polls `GetStatus(cop_handle)` until it reports `PduCopstExecuting`, or
/// panics after ~2s -- used to make a "sibling COP is actively executing"
/// test precondition structural rather than timing-lucky (mirrors
/// `cop_ctrl_cycles.rs`'s identical `wait_for_cop_status` helper, not shared
/// across files per this suite's existing per-file-duplication convention).
async fn wait_for_cop_executing(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cop_handle: ComPrimitiveHandle,
) {
    for _ in 0..200 {
        let response = client
            .get_status(GetStatusRequest {
                handle: Some(get_status_request::Handle::CopHandle(cop_handle)),
            })
            .await
            .expect("get_status(COP) should succeed")
            .into_inner();
        if let Some(status_response::Status::CopStatus(s)) = response.status
            && s == PduComPrimitiveStatus::PduCopstExecuting as i32
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("cop_handle {cop_handle:?} did not reach PduCopstExecuting within ~2s");
}

/// `PduErrEvtRscLocked` predicate for [`wait_for_event`] (ADR-110).
fn is_rsc_locked_error(item: &vci_service_interface::EventItem) -> bool {
    matches!(
        item.data,
        Some(event_item::Data::ErrorData(error))
            if error == vci_service_interface::PduErrorEvent::PduErrEvtRscLocked as i32
    )
}

/// Verifies ISO 22900-2 §9.3.3.6: `PDU_PC_UNIQUE_ID` class ComParams (per-ECU
/// addressing such as `CP_CanPhysReqId` / `CP_CanRespUSDTId` / `CP_CanRespUUDTId`)
/// are rejected by `SetComParam`, even on the protocol family they otherwise
/// belong to (ISO15765/CAN) — they may only be set through
/// `SetUniqueRespIdTable`, which is unaffected (see the tests below).
#[tokio::test]
#[serial]
async fn set_com_param_rejects_unique_id_class_params() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    for &com_param_id in &[CP_CAN_PHYS_REQ_ID, CP_CAN_RESP_USDT_ID, CP_CAN_RESP_UUDT_ID] {
        let status = client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                    com_param_class: vci_service_interface::PduParamClass::PduPcUniqueId as i32,
                    param_data: Some(param_item::ParamData::Unum32(0x7E0)),
                }),
            })
            .await
            .expect_err(&format!(
                "SetComParam({com_param_id:#x}) should reject a PDU_PC_UNIQUE_ID class param"
            ));
        assert_eq!(
            status.code(),
            tonic::Code::InvalidArgument,
            "com_param_id {com_param_id:#x}"
        );
    }

    // CP_J1939SourceName (0x8096) is the one PDU_PC_UNIQUE_ID class param with a
    // Bytefield value; it goes through a separate match arm in rpc_set_com_param
    // that used to (redundantly, and now unreachably) list it as accepted.
    const CP_J1939_SOURCE_NAME: u32 = 0x8096;
    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    CP_J1939_SOURCE_NAME,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcUniqueId as i32,
                param_data: Some(param_item::ParamData::Bytefield(vec![0; 8])),
            }),
        })
        .await
        .expect_err("SetComParam(CP_J1939SourceName) should reject a PDU_PC_UNIQUE_ID class param");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// Verifies the reverse direction of the same class boundary: `SetUniqueRespIdTable`
/// must reject entries containing a ComParam that is *not* `PDU_PC_UNIQUE_ID` class
/// for the CLL's protocol (e.g. `CP_Baudrate`, a regular ComParam) — the table is
/// specifically for per-ECU addressing params (ISO 22900-2 §9.3.3.6), not a second,
/// unchecked path for arbitrary ComParams.
#[tokio::test]
#[serial]
async fn set_unique_resp_id_table_rejects_non_unique_id_class_params() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let status = client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![EcuUniqueRespData {
                    unique_resp_identifier: 1,
                    params: vec![unum32_param(j2534_0404::DATA_RATE, 500_000)],
                }],
            }),
        })
        .await
        .expect_err("SetUniqueRespIdTable should reject a non-PDU_PC_UNIQUE_ID class param");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// Verifies ADR-043 (amended by ADR-068): `SetUniqueRespIdTable` respects
/// `LOCK_PHYSICAL_COM_PARAMS` on ISO15765 channels at Set/stage time -- even
/// though the actual `FLOW_CONTROL_FILTER` hardware I/O (ADR-039) now
/// happens later, at promotion time (this CLL's own `ConnectComLogicalLink`,
/// still gated by this lock for a brand-new channel, ADR-045; or
/// `CoptUpdateparam`, which -- per ADR-110 -- is no longer synchronously
/// gated by this lock at all, instead resolving any live conflict as a
/// `PDU_ERR_EVT_RSC_LOCKED` error event at execution time) -- so a CLL
/// sharing the channel still cannot stage a table change while another CLL
/// holds the lock, even though the later promotion step itself is handled
/// differently depending on which RPC performs it.
#[tokio::test]
#[serial]
async fn set_unique_resp_id_table_respects_physical_com_param_lock() {
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Two CLLs sharing one ISO15765/500kbps physical channel.
    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect("lock_resource should succeed");

    let status = client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_b),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![EcuUniqueRespData {
                    unique_resp_identifier: 1,
                    params: vec![
                        unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                        unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
                    ],
                }],
            }),
        })
        .await
        .expect_err(
            "SetUniqueRespIdTable should be blocked by another CLL's physical ComParam lock",
        );
    assert_eq!(status.code(), tonic::Code::ResourceExhausted);

    // The lock holder itself is unaffected.
    set_unique_resp_table(
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

    server.shutdown().await;
}

/// Verifies ADR-110 (ISO 22900-2 §9.4.16 d), superseding ADR-044's
/// synchronous rejection): a non-owning CLL's `CoptUpdateparam` is never
/// rejected by another CLL's `LOCK_PHYSICAL_COM_PARAMS`. `cll_b` joins
/// `cll_a`'s already-open physical channel, so its Active stays default
/// (empty) until it issues `CoptUpdateparam` (a pre-existing,
/// separately-documented quirk -- see ADR-060's Consequences); it stages
/// `CP_Baudrate` (a `PDU_PC_BUSTYPE`-class param, differing from its own
/// empty Active) plus `CP_RequestAddrMode` (not BUSTYPE class) in Working.
/// When `cll_a` holds the lock, `cll_b`'s `CoptUpdateparam` must still
/// return a COP handle, emit exactly one `PDU_ERR_EVT_RSC_LOCKED` error
/// event (this CLL's own Working genuinely differs from its own Active on
/// `CP_Baudrate`, so it did attempt a BUSTYPE change), and finish
/// (`PDU_COPST_FINISHED`, not `Cancelled`) -- with `CP_Baudrate` excluded
/// from the hardware push (`apply_bustype_lock`'s `hw_set`) and its
/// promotion (`promote_set`) pinned at cll_b's own pre-conflict (empty)
/// Active value, while `CP_RequestAddrMode` (non-BUSTYPE) promotes normally
/// in both. See `start_com_primitive_updateparam_stale_own_active_does_not_
/// clobber_real_hardware_and_fires_no_event` below for the corrected
/// design's other case: own Working == own (but hardware-stale) Active,
/// which excludes from `hw_set` just the same but fires no event.
#[tokio::test]
#[serial]
async fn start_com_primitive_updateparam_creates_cop_and_reports_rsc_locked_event_on_lock_conflict()
{
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Two CLLs sharing one CAN/500kbps physical channel.
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

    // A second, non-BUSTYPE-class param staged on cll_b, so the test can
    // also confirm it DOES promote despite the lock conflict on CP_Baudrate.
    set_com_param_unum32(&mut client, cll_b, CP_REQUEST_ADDR_MODE, 2).await;

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect("lock_resource should succeed");

    // Subscribe BEFORE starting CoptUpdateparam so its events cannot be missed.
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: Vec::new(),
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "CoptUpdateparam should return a COP handle even when another CLL holds the \
             physical ComParam lock (ADR-110)",
        );

    assert!(
        wait_for_event(&mut events, 2000, is_rsc_locked_error).await,
        "expected a PduErrEvtRscLocked error event"
    );
    wait_for_cop_finished(&mut events).await;
    assert!(
        !wait_for_event(&mut events, 300, is_rsc_locked_error).await,
        "expected exactly one PduErrEvtRscLocked error event, not a second one"
    );

    drop(events);

    // Verify promotion outcome: CoptRestoreParam copies Active back into
    // Working (ADR-067), so GetComParam -- which always reads Working
    // (ADR-067 claim F) -- can observe what actually promoted to Active.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptRestoreParam as i32,
            cop_data: Vec::new(),
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptRestoreParam) should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    assert_eq!(
        get_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE).await,
        0,
        "CP_Baudrate is BUSTYPE-class and conflicted with cll_a's lock -- it must stay pinned \
         at cll_b's pre-conflict (empty/default) Active value, not promote to the staged 500_000"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_b, CP_REQUEST_ADDR_MODE).await,
        2,
        "CP_RequestAddrMode is not BUSTYPE-class and did not conflict -- it must still promote"
    );

    server.shutdown().await;
}

/// Regression test for a confirmed design flaw in this ADR's first cut
/// (edge-case-hunter repro, corrected by the three-role `apply_bustype_lock`
/// split): a non-owning CLL's own Working-vs-Active agreement on a BUSTYPE
/// key is NOT proof that pushing it to hardware is safe -- `LogicalLinkState
/// ::active` is per-CLL bookkeeping with no cross-CLL sync, so it can go
/// stale relative to what the lock holder has since pushed to the real
/// shared hardware, with no local signal of that staleness. `cll_b` syncs
/// Working == Active once (unlocked), `cll_a` then changes the real hardware
/// value and acquires the lock, and `cll_b`'s OWN Working/Active never
/// change again -- so `cll_b`'s next `CoptUpdateparam` has nothing of its
/// own to "attempt": no event fires. But the corrected design still excludes
/// the BUSTYPE key from the hardware push (`hw_set`) unconditionally
/// whenever locked, so `cll_a`'s real hardware value survives regardless.
#[tokio::test]
#[serial]
async fn start_com_primitive_updateparam_stale_own_active_does_not_clobber_real_hardware_and_fires_no_event()
 {
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a is first on the channel: its initial CP_BitSamplePoint=80 is
    // pushed to real hardware at ConnectComLogicalLink time (ADR-045).
    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::BIT_SAMPLE_POINT, 80),
        ],
    )
    .await;
    let cll_b = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::BIT_SAMPLE_POINT, 80),
        ],
    )
    .await;

    // cll_b joins cll_a's already-open channel (its Active stays default
    // empty until it issues CoptUpdateparam -- ADR-060's Consequences); sync
    // it once, unlocked, so its own Working == its own Active afterward.
    promote_via_update_param(&mut client, cll_b).await;
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BIT_SAMPLE_POINT),
        80
    );

    // cll_a changes the real hardware value -- cll_b's own Active is now
    // stale relative to hardware, with no local signal of that.
    set_com_param_unum32(&mut client, cll_a, j2534_0404::BIT_SAMPLE_POINT, 60).await;
    promote_via_update_param(&mut client, cll_a).await;
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BIT_SAMPLE_POINT),
        60,
        "cll_a's own CoptUpdateparam must have actually pushed the new value to hardware"
    );

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect("lock_resource should succeed");

    // Subscribe BEFORE starting CoptUpdateparam so a (wrongly) fired event
    // cannot be missed.
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // cll_b's own Working still equals its own (stale) Active -- 80 == 80 --
    // so it stages nothing new here; this call must not clobber cll_a's real
    // hardware value of 60.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: Vec::new(),
            cop_ctrl_data: None,
        })
        .await
        .expect("CoptUpdateparam should succeed even when another CLL holds the lock (ADR-110)");
    wait_for_cop_finished(&mut events).await;

    assert!(
        !wait_for_event(&mut events, 300, is_rsc_locked_error).await,
        "cll_b's own Working never differed from its own Active -- it attempted no BUSTYPE \
         change, so no PduErrEvtRscLocked event should fire"
    );
    drop(events);

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BIT_SAMPLE_POINT),
        60,
        "cll_b's stale own-Active value (80) must NOT have clobbered cll_a's real, currently \
         -locked hardware value (60) -- the confirmed regression this test pins"
    );

    server.shutdown().await;
}

/// Verifies ADR-110 (superseding ADR-044's `temp_param_update` rejection,
/// per the amendment note added to ADR-067 §H): `CoptSendrecv`/`CoptStartcomm`
/// with `temp_param_update` set is no longer gated by
/// `LOCK_PHYSICAL_COM_PARAMS` at all -- ADR-067 §E's own
/// `PDU_ERR_TEMPPARAM_NOT_ALLOWED` BUSTYPE-differ guard is the sole call-time
/// physical-ComParam gate for these calls, and it is unaffected by lock
/// state. This test syncs `cll_b`'s Active to its Working (via
/// `CoptUpdateparam`, itself unaffected by the lock per ADR-110) before
/// acquiring the lock, specifically so ADR-067 §E's guard does not itself
/// reject the call -- isolating the assertion to the (now-removed) lock
/// check alone.
#[tokio::test]
#[serial]
async fn start_com_primitive_temp_param_update_is_not_gated_by_physical_com_param_lock() {
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;

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

    // Stage physical addressing (CP_CanPhysReqId) -- required for CoptSendrecv
    // to resolve a TX header at all, regardless of this test's own lock
    // question.
    set_can_phys_req_id(&mut client, cll_b, 0x7E0).await;

    // cll_b joins cll_a's already-open physical channel, so its Active stays
    // default (empty) until it issues CoptUpdateparam (a pre-existing,
    // separately-documented quirk -- see ADR-060's Consequences). Sync it
    // here so ADR-067 §E's BUSTYPE guard (which compares Working vs Active
    // on CP_Baudrate, among others) does not itself reject the
    // temp_param_update call below -- isolating this test to the
    // LOCK_PHYSICAL_COM_PARAMS question alone. This also promotes the
    // just-staged UniqueRespIdTable to Active (ADR-068).
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: Vec::new(),
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptUpdateparam) should succeed for cll_b");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // cll_a holds only the physical ComParam lock, not the TX queue lock.
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect("lock_resource should succeed");

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
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
        .expect(
            "CoptSendrecv with temp_param_update should succeed even when another CLL holds \
             the physical ComParam lock (ADR-110) -- only ADR-067 §E's BUSTYPE-differ guard, \
             synced away above, could still reject it",
        );

    server.shutdown().await;
}

// Regression-test feasibility note for ADR-110's "Lock-grant/apply
// serialization" amendment (Finding 2, Codex review on PR #116): a
// deterministic test proving the fix -- issuing an unlocked `cll_b`
// `CoptUpdateparam` staging a BUSTYPE diff, then racing a `cll_a`
// `LockResource` call against it, and observing that the grant either sees
// the completed push or blocks until it's done, never both "grant returns
// success" and "a stale unfiltered push is still in flight" -- was
// attempted and confirmed infeasible with this harness, not just assumed
// so.
//
// A throwaway diagnostic test doing exactly that race (issue
// `CoptUpdateparam` with no intervening `.await`, immediately issue
// `LockResource`, then inspect the hardware value and whether
// `PDU_ERR_EVT_RSC_LOCKED` fired) was run five times against the fixed
// code: every run produced the IDENTICAL outcome (`lock_resource` succeeds,
// no `PDU_ERR_EVT_RSC_LOCKED`, and the hardware value is the FULL staged
// value) -- i.e. `cll_b`'s push always completes entirely before `cll_a`'s
// grant is even attempted, on every run. This is the same "this crate's
// `current_thread` `#[tokio::test]` harness with real timers produces a
// highly reproducible relative task-scheduling order" finding already
// documented for several other narrow lock-acquisition races (see
// `cop_ctrl_cycles.rs`'s own `tier1_wait_...many_times` doc comment): with
// no intervening real (unmocked, cooperatively-yielding) delay to force the
// two client-issued RPCs to genuinely interleave, `StartComPrimitive`
// simply enqueues the `CoptUpdateparam` work item and returns; the poll
// task's own `handle_update_param` -- an uncontended `ctx.api.lock().await`
// followed by one synchronous, near-instant mock `SET_CONFIG` call -- runs
// to completion before the driving test task's next `.await`
// (`lock_resource`) ever reaches the wire, deterministically, every time.
//
// The existing `arm_write_rx_injection` hold hook (this crate's only
// "make a hardware call take real wall-clock time" mechanism) cannot
// substitute: it blocks via `std::thread::sleep` inside the mock's C ABI,
// which stalls the ENTIRE single OS thread this `current_thread` runtime
// uses -- not just the task holding the lock. During that block NOTHING
// else runs, including the task that would need to attempt the competing
// `LockResource` call, so it cannot be used to create a genuine two-task
// interleaving window either (this is the "synchronous-FFI-under-an-async-
// executor defeats timing-based proofs" finding referenced by this fix's
// own brief); and there is no equivalent hold hook for `SET_CONFIG`
// specifically. Constructing one would be a new test-only production-code
// pause/gate hook -- explicitly out of scope for this fix, matching this
// codebase's established precedent for this exact class of window (see
// `docs/implementation-notes.md`'s round-4/5/6/8 ADR-086 notes and the
// `handle_stop_comm` S3-guard note referenced there).
//
// The fix is instead verified by this file's and `startcomm_comparam.rs`'s
// full existing ADR-110 regression suite passing unchanged (no deadlock,
// no behavior change in every ordering this harness CAN construct — i.e.
// `LockResource` fully before `CoptUpdateparam`, per the test above, and
// `CoptUpdateparam` fully before `LockResource`, per this note's own
// diagnostic run), plus the crate-wide `api`-before-`logical_links`
// lock-ordering-invariant sweep recorded in ADR-110's own amendment
// section.

/// Verifies ADR-045: the first `ConnectComLogicalLink` on a brand-new physical
/// channel performs a real hardware write (`PassThruConnect` +
/// `PassThruIoctl SET_CONFIG`), so it respects `LOCK_PHYSICAL_COM_PARAMS` too --
/// using the same protocol-only pre-connect fallback `LockResource` itself uses,
/// since no `channel_key` exists yet for a not-yet-connected CLL to match on more
/// precisely. This closes the case where a second CLL bypasses a held lock by
/// connecting a brand-new channel (e.g. a different baud rate defaulted from its
/// bustype) that never goes through the lock-checked `SetComParam` path at all.
#[tokio::test]
#[serial]
async fn connect_com_logical_link_respects_physical_com_param_lock_for_new_channel() {
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    // cll_a: create but do not connect, then reserve the physical ComParam lock
    // pre-connect (LockResource's protocol-only fallback).
    let cll_a = create_cll(&mut client, j2534_0404::CAN, 1).await;

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect("lock_resource should succeed");

    // cll_b: a second CLL on the same J2534 protocol, named with a bustype so its
    // default DATA_RATE comes from bustype_default_params -- never through
    // SetComParam, which cll_a's lock would otherwise have blocked directly.
    let cll_b = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_and_bustype(j2534_0404::CAN, "iso_11898_2_dwcan"),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect_err(
            "ConnectComLogicalLink should be blocked by another CLL's physical ComParam lock",
        );
    assert_eq!(status.code(), tonic::Code::ResourceExhausted);

    // The lock holder itself is unaffected.
    set_com_param_unum32(&mut client, cll_a, j2534_0404::DATA_RATE, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("connect_com_logical_link should succeed for the lock holder");

    server.shutdown().await;
}

// ── ADR-123 (A2-15): LockResource/UnlockResource conformance fixes ──────────

/// Fix 1 (ADR-123): `UnlockResource` rejects a bit this CLL never held with
/// `PDU_ERR_RSC_NOT_LOCKED` / `FailedPrecondition` (spec 9.4.14.5), not a
/// silent no-op.
#[tokio::test]
#[serial]
async fn unlock_resource_rejects_a_never_locked_bit_with_rsc_not_locked() {
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let status = client
        .unlock_resource(UnlockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect_err("unlock_resource should reject a bit this CLL never locked");
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrRscNotLocked as i32
    );

    server.shutdown().await;
}

/// Fix 1 (ADR-123): `UnlockResource` rejects a bit held by ANOTHER CLL with
/// `PDU_ERR_RSC_LOCKED_BY_OTHER_CLL` / `ResourceExhausted` (spec 9.4.14.5) --
/// this code is legal for `PDUUnlockResource` (Table 22), unlike
/// `PDULockResource` (see the FCT_FAILED/RSC_LOCKED test below).
#[tokio::test]
#[serial]
async fn unlock_resource_rejects_a_bit_held_by_another_cll_with_rsc_locked_by_other_cll() {
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;

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

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect("lock_resource should succeed");

    let status = client
        .unlock_resource(UnlockResourceRequest {
            cll_handle: Some(cll_b),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect_err("unlock_resource should reject a bit held by another CLL");
    assert_eq!(status.code(), tonic::Code::ResourceExhausted);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrRscLockedByOtherCll as i32
    );

    server.shutdown().await;
}

/// Fix 2 (ADR-123): `LockResource(LOCK_PHYSICAL_TX_QUEUE)` is rejected with
/// `PDU_ERR_FCT_FAILED` / `FailedPrecondition` while another CLL sharing the
/// physical resource has an ACTIVE transmission (a live `executing_cop`) in
/// flight -- Table 21 offers no better code for a busy-not-locked resource
/// (`RSC_LOCKED`'s "already in the locked state" description does not
/// apply). Also confirms the renamed conflicting-bit rejection separately
/// (see `set_unique_resp_id_table_respects_physical_com_param_lock` and
/// friends for the pre-existing "another CLL already holds the bit" cases,
/// now `PDU_ERR_RSC_LOCKED`).
#[tokio::test]
#[serial]
async fn lock_resource_rejects_active_transmission_conflict_with_fct_failed() {
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
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 500_000), // 500ms window -- ample real time to observe.
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    // cll_b's CoptSendrecv occupies the poll task inside its receive-phase
    // wait for (nearly) its whole 500ms CP_P2Max window -- nothing ever
    // answers, so `executing_cop` stays set to this COP the whole time.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // cll_a shares the physical resource but is not the owner of the
    // currently-executing COP -- LockResource(TX_QUEUE) must be rejected.
    let status = client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect_err(
            "lock_resource should be rejected while another CLL's COP is actively transmitting",
        );
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrFctFailed as i32
    );

    server.shutdown().await;
}

/// Fix 3 (ADR-123): a `CoptSendrecv` from a CLL that does NOT hold
/// `LOCK_PHYSICAL_TX_QUEUE` is now QUEUED (not rejected) while another CLL
/// on the same physical resource holds it, and dispatches once that CLL
/// releases the lock via `UnlockResource` -- replacing the old
/// `StartComPrimitive`-level hard reject.
#[tokio::test]
#[serial]
async fn sendrecv_from_a_non_holding_cll_queues_and_executes_once_the_lock_releases() {
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

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
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
        .expect(
            "CoptSendrecv should be accepted (queued), not rejected, while another CLL holds \
             the TX queue lock (ADR-123)",
        );
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the queued CoptSendrecv must not transmit while cll_a holds the TX queue lock"
    );

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

/// Fix 3b (ADR-123): a CLL that `ConnectComLogicalLink`s onto a physical
/// channel a sibling already holds `LOCK_PHYSICAL_TX_QUEUE` on starts
/// suspended immediately (ISO 22900-2 §9.4.13.3 use case 1: ComLogicalLinks
/// created afterwards start with their ComPrimitive queue in
/// SUSPEND_TX_QUEUE mode) -- its queued COP must not transmit until the
/// lock releases.
#[tokio::test]
#[serial]
async fn newly_connected_cll_starts_suspended_when_a_sibling_already_holds_the_tx_queue_lock() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("lock_resource should succeed");

    // cll_b joins cll_a's already-open CAN/500kbps physical channel AFTER
    // the lock is granted. Addressing is staged and connected BEFORE
    // `ConnectComLogicalLink` rather than promoted afterward via
    // `CoptUpdateparam`: once connected, cll_b starts suspended (this test's
    // own point), and `CoptUpdateparam` is itself a TxItem that would sit
    // held in `tx_held` just like `CoptSendrecv` below -- `ConnectComLogicalLink`
    // promotes Working -> Active for THIS CLL synchronously, with no COP/TxItem
    // involved (ADR-068), so staging before connect avoids that self-deadlock.
    let cll_b = create_cll(&mut client, j2534_0404::CAN, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_can_phys_req_id(&mut client, cll_b, 0x7E0).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
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
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "cll_b must start suspended: cll_a already held LOCK_PHYSICAL_TX_QUEUE on this \
         physical resource at cll_b's connect time"
    );

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

/// Fix 4 (ADR-123): both `LockResource` and `UnlockResource` reject an
/// all-zero `lock_mask` and any mask containing a bit outside
/// `LOCK_PHYSICAL_COM_PARAMS | LOCK_PHYSICAL_TX_QUEUE` -- even mixed with a
/// defined bit -- with `PDU_ERR_INVALID_PARAMETERS` / `InvalidArgument`,
/// rather than silently narrowing the mask.
#[tokio::test]
#[serial]
async fn lock_and_unlock_resource_reject_undefined_bit_and_zero_masks() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // 0: all-zero. 0x04: a single undefined bit. 0x05: a defined bit (0x01)
    // mixed with an undefined one (0x04).
    for &bad_mask in &[0u32, 0x04, 0x05] {
        let status = client
            .lock_resource(LockResourceRequest {
                cll_handle: Some(cll_a),
                lock_mask: bad_mask,
            })
            .await
            .unwrap_err();
        assert_eq!(
            status.code(),
            tonic::Code::InvalidArgument,
            "lock_resource(lock_mask={bad_mask:#04x})"
        );
        assert_eq!(
            error_detail_from_status(&status).unwrap().pdu_error,
            PduError::PduErrInvalidParameters as i32,
            "lock_resource(lock_mask={bad_mask:#04x})"
        );

        let status = client
            .unlock_resource(UnlockResourceRequest {
                cll_handle: Some(cll_a),
                lock_mask: bad_mask,
            })
            .await
            .unwrap_err();
        assert_eq!(
            status.code(),
            tonic::Code::InvalidArgument,
            "unlock_resource(lock_mask={bad_mask:#04x})"
        );
        assert_eq!(
            error_detail_from_status(&status).unwrap().pdu_error,
            PduError::PduErrInvalidParameters as i32,
            "unlock_resource(lock_mask={bad_mask:#04x})"
        );
    }

    server.shutdown().await;
}

// ── ADR-123 (A2-15) coverage gaps (edge-case-hunter verification pass) ─────

/// Coverage gap: `DisconnectComLogicalLink` clears `held_lock_mask`
/// unconditionally (not gated on the CLL itself calling `UnlockResource`) --
/// it must still resume a suspended sibling's `tx_held` backlog via the
/// `recompute_lock_tx_suspensions` sweep, exactly like an explicit
/// `UnlockResource` does. Setup mirrors `sendrecv_from_a_non_holding_cll_
/// queues_and_executes_once_the_lock_releases`, just releasing the lock via
/// disconnect instead.
#[tokio::test]
#[serial]
async fn disconnect_com_logical_link_resumes_a_lock_suspended_siblings_queued_cop() {
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

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
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
        .expect(
            "CoptSendrecv should be accepted (queued), not rejected, while another CLL holds \
             the TX queue lock (ADR-123)",
        );
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the queued CoptSendrecv must not transmit while cll_a holds the TX queue lock"
    );

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.shutdown().await;
}

/// Coverage gap: same as
/// `disconnect_com_logical_link_resumes_a_lock_suspended_siblings_queued_cop`
/// above, but releasing the lock via `DestroyComLogicalLink` instead --
/// `rpc_destroy_com_logical_link` removes the CLL from `logical_links`
/// entirely and then runs the same `recompute_lock_tx_suspensions` sweep.
#[tokio::test]
#[serial]
async fn destroy_com_logical_link_resumes_a_lock_suspended_siblings_queued_cop() {
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

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
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
        .expect(
            "CoptSendrecv should be accepted (queued), not rejected, while another CLL holds \
             the TX queue lock (ADR-123)",
        );
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the queued CoptSendrecv must not transmit while cll_a holds the TX queue lock"
    );

    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("destroy_com_logical_link should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    server.shutdown().await;
}

/// Coverage gap (ADR-123 §2): the active-transmission check applies only
/// when `LOCK_PHYSICAL_TX_QUEUE` is requested -- per spec use case 2 (spec
/// line 1814), a ComParam lock "will not terminate any ongoing
/// transmissions." `LockResource(LOCK_PHYSICAL_COM_PARAMS)` must succeed
/// despite another CLL's actively-executing COP on the same physical
/// resource. Reuses `lock_resource_rejects_active_transmission_conflict_with_
/// fct_failed`'s exact "active transmission" setup above, requesting a
/// different lock bit.
#[tokio::test]
#[serial]
async fn lock_resource_com_params_only_skips_the_active_transmission_check() {
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;

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
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 500_000), // 500ms window -- ample real time to observe.
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    // cll_b's CoptSendrecv occupies the poll task inside its receive-phase
    // wait for (nearly) its whole 500ms CP_P2Max window -- nothing ever
    // answers, so `executing_cop` stays set to this COP the whole time.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // cll_a shares the physical resource and requests only the ComParams
    // lock -- the grant must succeed despite cll_b's active transmission.
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect(
            "lock_resource(LOCK_PHYSICAL_COM_PARAMS) must succeed despite another CLL's active \
             transmission -- the active-transmission check applies only to \
             LOCK_PHYSICAL_TX_QUEUE (ADR-123 use case 2)",
        );

    server.shutdown().await;
}

/// Codex review Finding B (ADR-123): a non-transmitting item
/// (`TxItem::transmits() == false`, e.g. `CoptDelay`) from a CLL that does
/// NOT hold `LOCK_PHYSICAL_TX_QUEUE` executes immediately despite a
/// sibling's held lock -- `tx_suspended_by_lock` alone no longer siphons a
/// non-transmitting item, since a TX-queue lock only needs to hold up actual
/// bus traffic (§9.4.13.3 use case 1). Before this fix, the siphon checked
/// `link.tx_suspended()` unconditionally and held EVERY item regardless of
/// whether it would ever touch the bus. cll_a's lock is never released in
/// this test -- the delay must still reach `PduCopstFinished` on its own.
#[tokio::test]
#[serial]
async fn non_transmitting_item_from_a_non_holding_cll_is_not_held_by_a_siblings_tx_queue_lock() {
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
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptDelay as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 50,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect(
            "start_com_primitive(CoptDelay) should succeed while cll_a holds \
             LOCK_PHYSICAL_TX_QUEUE",
        );

    // If the item were still (wrongly) siphoned into tx_held, this would
    // time out -- cll_a's lock is never released in this test.
    wait_for_cop_finished(&mut events).await;

    drop(events);
    server.shutdown().await;
}

/// Codex review Finding C (ADR-123): a pre-connect
/// `LockResource(LOCK_PHYSICAL_TX_QUEUE)` -- requested by a CLL that has not
/// yet called `ConnectComLogicalLink` (no `channel_key` yet) -- still finds
/// and rejects against an already-connected sibling on the same
/// `hw_protocol_id` with an actively transmitting COP. Before this fix, the
/// busy check ran in a standalone block that resolved the physical channel
/// via `channel_key.and_then(|ck| chans.get(&ck))`, which is unconditionally
/// `None` pre-connect and skipped the check outright.
#[tokio::test]
#[serial]
async fn lock_resource_pre_connect_rejects_active_transmission_on_same_hw_protocol_id() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 500_000), // 500ms window -- ample real time to observe.
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;

    // cll_a's CoptSendrecv occupies the poll task inside its receive-phase
    // wait for (nearly) its whole 500ms CP_P2Max window -- nothing ever
    // answers, so `executing_cop` stays set to this COP the whole time.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // cll_b is CREATED but never connected -- no `channel_key` yet -- and
    // shares cll_a's hw_protocol_id (both CAN, set at CreateComLogicalLink
    // regardless of connect state). LockResource(TX_QUEUE) must still find
    // cll_a's active transmission via the hw_protocol_id fallback in
    // `same_physical_resource`.
    let cll_b = create_cll(&mut client, j2534_0404::CAN, 2).await;

    let status = client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_b),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect_err(
            "pre-connect lock_resource should be rejected while a same-hw_protocol_id sibling's \
             COP is actively transmitting",
        );
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrFctFailed as i32
    );

    server.shutdown().await;
}

/// Codex review round 5 Finding J (ADR-123): the busy check is
/// resource-scoped, not CLL-scoped -- it must reject even when the
/// requesting CLL is the SAME CLL that owns the pinned, actively-transmitting
/// executing COP. Before this fix, the check read `other != handle`, wrongly
/// exempting this case (ISO 22900-2:2009(E) §9.4.13.2 b)'s "other" qualifier
/// grammatically scopes only the locks clause, not the active-transmissions
/// clause).
#[tokio::test]
#[serial]
async fn lock_resource_rejects_active_transmission_owned_by_the_requesting_cll_itself() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 500_000), // 500ms window -- ample real time to observe.
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_a, 0x7E0).await;

    // cll_a's own CoptSendrecv occupies the poll task inside its
    // receive-phase wait for (nearly) its whole 500ms CP_P2Max window --
    // nothing ever answers, so `executing_cop` stays set to this COP, owned
    // by cll_a itself, the whole time.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // cll_a calls LockResource(TX_QUEUE) on ITSELF while its own COP is the
    // one actively transmitting -- must still be rejected.
    let status = client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect_err(
            "lock_resource should be rejected even when the requesting CLL is the same CLL \
             whose own COP is actively transmitting",
        );
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrFctFailed as i32
    );

    server.shutdown().await;
}

/// Codex review Finding D (ADR-123): a sibling's actively-executing
/// `CoptDelay` (`CopEntry::transmits == false`) does NOT block a
/// `LockResource(LOCK_PHYSICAL_TX_QUEUE)` grant -- only an executing COP that
/// actually transmits on the bus blocks the grant. Before this fix, the
/// busy check rejected on ANY resolved, different-CLL `executing_cop`,
/// without regard to whether that COP transmits.
#[tokio::test]
#[serial]
async fn lock_resource_is_not_blocked_by_a_siblings_non_transmitting_executing_cop() {
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

    // cll_b's CoptDelay occupies the poll task for 500ms without ever
    // transmitting -- ample real time to observe it PduCopstExecuting.
    let cop_handle = client
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
        .expect("start_com_primitive(CoptDelay) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    wait_for_cop_executing(&mut client, cop_handle).await;

    // cll_a shares the physical resource but does not hold the lock --
    // LockResource(TX_QUEUE) must succeed despite cll_b's actively-executing
    // (but non-transmitting) CoptDelay.
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect(
            "lock_resource(LOCK_PHYSICAL_TX_QUEUE) must succeed despite another CLL's actively \
             executing, non-transmitting CoptDelay",
        );

    server.shutdown().await;
}

/// Codex review round 2, Finding F (ADR-123): `drain_tx_held_backlog` now
/// pops and drains a non-transmitting FRONT item of `tx_held` while
/// `tx_suspended_by_lock` alone is set, in FIFO order, instead of refusing to
/// pop anything at all until the CLL's own suspension clears entirely.
///
/// cll_b siphons a `CoptDelay` then a `CoptSendrecv` (in that order) into
/// `tx_held` via its own `PDU_IOCTL_SUSPEND_TX_QUEUE`
/// (`tx_suspended_by_ioctl`, unconditional -- holds every item regardless of
/// `transmits()`). cll_a then acquires `LOCK_PHYSICAL_TX_QUEUE` on the same
/// physical resource, which recomputes cll_b's `tx_suspended_by_lock = true`
/// too. cll_b then issues `PDU_IOCTL_RESUME_TX_QUEUE`, clearing only
/// `tx_suspended_by_ioctl` -- `tx_suspended_by_lock` stays set, owned
/// exclusively by cll_a's held lock. The resulting `ResumeWake` drives
/// `drain_tx_held_backlog`: the front `Delay` (non-transmitting) must drain
/// and execute despite `tx_suspended_by_lock` still being true, while the
/// `SendRecv` behind it (transmitting) must stay held until cll_a calls
/// `UnlockResource`.
///
/// This test doubles as the livelock regression for Finding F's naive fix
/// (gating only `drain_tx_held_backlog`'s pop condition on
/// `!item.transmits()`, without also suppressing `dispatch_tx_item`'s
/// fresh-siphon FIFO no-overtake clause on the drain-re-entry path via
/// `is_backlog_drain`): with that naive fix, re-dispatching the popped Delay
/// would find `tx_held` still non-empty (the `SendRecv` still parked behind
/// it), re-siphon the Delay right back via `push_front`, and
/// `drain_tx_held_backlog`'s loop would pop the very same front item again
/// next iteration -- forever. If this test hangs rather than failing
/// cleanly, that is the strongest possible confirmation the
/// `is_backlog_drain` guard is load-bearing.
#[tokio::test]
#[serial]
async fn ioctl_resume_drains_leading_non_transmitting_backlog_item_under_lock_only_suspension() {
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
    // Both items enqueued below siphon into tx_held unconditionally under
    // this source (ADR-123 §3: tx_suspended_by_ioctl has no transmits-only
    // carve-out).
    io_ctl_cll(&mut client, cll_b, suspend_id)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Delay first, then SendRecv -- tx_held = [Delay, SendRecv] in FIFO
    // order once both have been siphoned.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptDelay as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 50,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptDelay) should succeed");
    start_send_recv(&mut client, cll_b, vec![0xAA]).await;

    // cll_a's TX-queue lock recomputes cll_b's tx_suspended_by_lock = true
    // too, since it shares the physical resource and does not hold the lock.
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("lock_resource should succeed");

    // Clearing only tx_suspended_by_ioctl must still drain the FRONT
    // non-transmitting Delay (Fix F) -- tx_suspended_by_lock alone no longer
    // blocks popping a non-transmitting front item.
    io_ctl_cll(&mut client, cll_b, resume_id)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    // The Delay must complete while cll_a's LOCK_PHYSICAL_TX_QUEUE is still
    // held -- if the livelock were present, this would hang instead.
    wait_for_cop_finished(&mut events).await;

    // The SendRecv behind it must stay held: nothing written yet.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "cll_b's SendRecv must stay held behind cll_a's LOCK_PHYSICAL_TX_QUEUE even after the \
         Delay ahead of it in tx_held has drained"
    );

    // Releasing cll_a's lock clears tx_suspended_by_lock too -- now the
    // SendRecv dispatches.
    client
        .unlock_resource(UnlockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("unlock_resource should succeed");
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    drop(events);
    server.shutdown().await;
}

/// Codex review round 4, Finding H (ADR-123): a receive-only `CoptSendrecv`
/// (`NumSendCycles == 0`, ADR-059) queued by a non-holding CLL must not be
/// held by a sibling's `LOCK_PHYSICAL_TX_QUEUE` -- `TxItem::transmits()` now
/// classifies `SendRecv` conditionally on `send_cycles_remaining != 0`,
/// instead of unconditionally `true`. Clones
/// `non_transmitting_item_from_a_non_holding_cll_is_not_held_by_a_siblings_tx_queue_lock`'s
/// shape (above), replacing that test's non-transmitting `CoptDelay` with a
/// non-transmitting, receive-only `CoptSendrecv`.
#[tokio::test]
#[serial]
async fn receive_only_sendrecv_from_a_non_holding_cll_is_not_held_by_a_siblings_tx_queue_lock() {
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
    // Even a receive-only COP resolves TX addressing at call time (the
    // TX message is composed and size-validated even though NumSendCycles ==
    // 0 means it is never actually written -- see `arm_receive_only_monitor`'s
    // doc comment in harness.rs for the same requirement).
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("lock_resource should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(vci_service_interface::subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect(
            "start_com_primitive(receive-only CoptSendrecv) should succeed while cll_a holds \
             LOCK_PHYSICAL_TX_QUEUE",
        );

    // If the item were still (wrongly) siphoned into tx_held, this would
    // time out -- cll_a's lock is never released in this test.
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "NumSendCycles == 0 must not transmit anything, lock or no lock"
    );

    drop(events);
    server.shutdown().await;
}

/// Codex review round 4, Finding H (ADR-123): a sibling's actively-executing,
/// receive-only `CoptSendrecv` (`NumSendCycles == 0`, so `CopEntry::transmits
/// == false` after this fix) does NOT block a
/// `LockResource(LOCK_PHYSICAL_TX_QUEUE)` grant -- mirrors
/// `lock_resource_is_not_blocked_by_a_siblings_non_transmitting_executing_cop`
/// (Fix D) above, but exercises the `SendRecv` variant's new conditional
/// classification rather than `CoptDelay`'s unconditional one.
///
/// Uses a FINITE `NumReceiveCycles` (`1`) with an empty expected-response
/// array and a generous `CP_P2Max`, not `NumReceiveCycles == -1`: a
/// created-receive-only (`NumSendCycles == 0`) COP with `NumReceiveCycles ==
/// -1` detaches to a tier-2 registrant and returns almost immediately (ADR-100
/// Decision §4, S6, `wait_for_expected_response`'s `is_receive_only_cyclic`
/// branch) -- `executing_cop` would be set and cleared again within the same
/// dispatch, giving no stable window to observe. A finite-N created-receive-
/// only COP is tier-2 from creation too, per ADR-100 round-9 Finding-1, but
/// still runs its own inline/blocking receive-phase loop for the whole of its
/// wait (same "mirrors `lock_resource_pre_connect_rejects_active_transmission_
/// on_same_hw_protocol_id`'s already-established CP_P2Max-blocking pattern"
/// shape used elsewhere in this file), so `executing_cop` stays set for the
/// whole ~500ms window with no timing race.
#[tokio::test]
#[serial]
async fn lock_resource_is_not_blocked_by_a_siblings_receive_only_executing_sendrecv() {
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
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (j2534_0404::P2_MAX, 500_000), // 500ms window -- ample real time to observe.
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_b, 0x7E0).await;

    // cll_b's receive-only CoptSendrecv occupies the poll task inside its
    // receive-phase wait for (nearly) its whole 500ms CP_P2Max window --
    // nothing ever answers (empty expected_response_array), so
    // `executing_cop` stays set to this COP the whole time, and
    // NumSendCycles == 0 means nothing is ever transmitted.
    let cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x3E, 0x00],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(receive-only CoptSendrecv) should succeed")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    wait_for_cop_executing(&mut client, cop_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "NumSendCycles == 0 must not transmit anything"
    );

    // cll_a shares the physical resource but does not hold the lock --
    // LockResource(TX_QUEUE) must succeed despite cll_b's actively-executing
    // (but non-transmitting, receive-only) CoptSendrecv.
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect(
            "lock_resource(LOCK_PHYSICAL_TX_QUEUE) must succeed despite another CLL's actively \
             executing, receive-only CoptSendrecv",
        );

    // Cancel the still-executing monitor so server shutdown does not wait out
    // its whole CP_P2Max window.
    client
        .cancel_com_primitive(CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    server.shutdown().await;
}

// ── A2-17/A2-18 (ADR-133): GetComParam class/oneof-variant fixes ────────────

/// Reads the raw `ParamData` for `com_param_id` via `GetComParam` -- unlike
/// `get_com_param_unum32` (harness.rs), this does not panic on a non-Unum32
/// response. Used by the A2-17/A2-18 tests below, which assert on
/// `com_param_class` and Bytefield/Structfield variants directly.
async fn get_com_param_item(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: ComLogicalLinkHandle,
    com_param_id: u32,
) -> ParamItem {
    client
        .get_com_param(vci_service_interface::GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(vci_service_interface::get_com_param_request::Param::ParamId(com_param_id)),
        })
        .await
        .unwrap_or_else(|err| panic!("get_com_param({com_param_id:#x}) should succeed: {err}"))
        .into_inner()
        .param_item
        .unwrap_or_else(|| panic!("get_com_param({com_param_id:#x}) response missing param_item"))
}

/// Creates and connects a CLL from a resources-table `resource_id` (rather
/// than a bare J2534 protocol id) -- needed to reach a `protocol_name` preset
/// (e.g. `ISO_14230_3_on_ISO_15765_2`, resource 0x0204) that
/// `create_and_connect_cll`'s `resource_with_protocol` cannot select, since
/// that helper only ever sets a raw `protocol_id` with no `bus_type` /
/// `protocol_name`.
async fn create_and_connect_cll_by_resource_id(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource_id: u32,
) -> ComLogicalLinkHandle {
    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::ResourceId(
                resource_id,
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    cll_handle
}

/// A2-17: `GetComParam` must report the correct `PDU_PC_*` class instead of
/// always hardcoding `PDU_PC_SPECIFIED` -- `CP_Baudrate` (`BUSTYPE_UNUM32`
/// member) reports `PDU_PC_BUSTYPE`, `CP_TesterPresentHandling` (a
/// `CP_TesterPresentxxx`-named param, ISO 22900-2 Table B.6) reports
/// `PDU_PC_TESTER_PRESENT`, and an ordinary param like `CP_RC21Handling` is
/// unchanged at `PDU_PC_SPECIFIED` (0).
#[tokio::test]
#[serial]
async fn get_com_param_reports_correct_param_class() {
    const CP_TESTER_PRESENT_HANDLING: u32 = 0x8006;
    const CP_RC21_HANDLING: u32 = 0x8021;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let item = get_com_param_item(&mut client, cll_handle, j2534_0404::DATA_RATE).await;
    assert_eq!(
        item.com_param_class,
        vci_service_interface::PduParamClass::PduPcBustype as i32,
        "CP_Baudrate is a BUSTYPE_UNUM32 member -- GetComParam must report PDU_PC_BUSTYPE"
    );

    let item = get_com_param_item(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING).await;
    assert_eq!(
        item.com_param_class,
        vci_service_interface::PduParamClass::PduPcTesterPresent as i32,
        "CP_TesterPresentHandling matches the CP_TesterPresentxxx naming rule -- GetComParam \
         must report PDU_PC_TESTER_PRESENT"
    );

    let item = get_com_param_item(&mut client, cll_handle, CP_RC21_HANDLING).await;
    assert_eq!(
        item.com_param_class,
        vci_service_interface::PduParamClass::PduPcSpecified as i32,
        "CP_RC21Handling is neither BUSTYPE nor TESTER_PRESENT class -- GetComParam must still \
         report the unchanged PDU_PC_SPECIFIED default"
    );

    server.shutdown().await;
}

/// A2-18: an unseeded Bytefield-typed ComParam other than
/// `CP_CanBaudrateRecord` (already covered by the ADR-130 regression test in
/// `resources.rs`) must also report an empty `Bytefield`, not the generic
/// `Unum32(0)` fallback. `CP_J1939Name` is `is_can_param`-allowed but not
/// seeded by the `ISO_14230_3_on_ISO_15765_2` preset (resource 0x0204, a
/// plain UDS-on-CAN diagnostic preset with no J1939 addressing).
#[tokio::test]
#[serial]
async fn get_com_param_unseeded_bytefield_reports_empty_bytefield() {
    const CP_J1939_NAME: u32 = 0x8094;
    const ISO_14230_3_ON_ISO_15765_2_RESOURCE_ID: u32 = 0x0204;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle =
        create_and_connect_cll_by_resource_id(&mut client, ISO_14230_3_ON_ISO_15765_2_RESOURCE_ID)
            .await;

    let item = get_com_param_item(&mut client, cll_handle, CP_J1939_NAME).await;
    assert_eq!(
        item.param_data,
        Some(vci_service_interface::param_item::ParamData::Bytefield(
            Vec::new()
        )),
        "CP_J1939Name is allowed but unseeded on this preset -- GetComParam must report an \
         empty Bytefield, not fall through to the Unum32(0) default (A2-18, pre-fix this \
         returned Unum32(0))"
    );

    server.shutdown().await;
}

/// A2-18: an unseeded Structfield-typed ComParam must report the correct
/// empty shape for its variant, not `Unum32(0)`.
///
/// `CP_ExtendedTiming` moved from `is_can_param` to ISO14230-specifically
/// (ADR-146 -- ISO 22900-2's own default-by-protocol tables scope it, and
/// its `CP_AccessTiming_Ecu`/`CP_AccessTimingOverride` siblings, to
/// ISO_14230_2/ISO_14230_4 only, correcting the prior backwards CAN-family
/// allow-listing); a bare/raw ISO14230 CLL (no resource-table preset) allows
/// it but seeds nothing, giving the unseeded shape this test exercises.
/// `CP_SessionTimingOverride` is unaffected by ADR-146 and is seeded only by
/// the `ISO_15765_3` family (resources 0x0202/0x0203/0x0207);
/// `ISO_14230_3_on_ISO_15765_2` (resource 0x0204) allows it (`is_can_param`)
/// but does not seed it.
#[tokio::test]
#[serial]
async fn get_com_param_unseeded_structfield_reports_correct_empty_shape() {
    const CP_EXTENDED_TIMING: u32 = 0x8050;
    const CP_SESSION_TIMING_OVERRIDE: u32 = 0x8016;
    const ISO_14230_3_ON_ISO_15765_2_RESOURCE_ID: u32 = 0x0204;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let kwp_cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400)],
    )
    .await;

    let item = get_com_param_item(&mut client, kwp_cll_handle, CP_EXTENDED_TIMING).await;
    assert_eq!(
        item.param_data,
        Some(vci_service_interface::param_item::ParamData::Structfield(
            vci_service_interface::ParamStructfield {
                data: Some(
                    vci_service_interface::param_structfield::Data::AccessTiming(
                        vci_service_interface::ParamAccessTimingList { entries: vec![] },
                    ),
                ),
            }
        )),
        "CP_ExtendedTiming is allowed but unseeded on a bare/raw ISO14230 CLL -- GetComParam \
         must report an empty AccessTiming Structfield (matching access_timing_empty()'s zero- \
         entry shape, not access_timing_zero()'s one-entry shape), not Unum32(0)"
    );

    let can_cll_handle =
        create_and_connect_cll_by_resource_id(&mut client, ISO_14230_3_ON_ISO_15765_2_RESOURCE_ID)
            .await;

    let item = get_com_param_item(&mut client, can_cll_handle, CP_SESSION_TIMING_OVERRIDE).await;
    assert_eq!(
        item.param_data,
        Some(vci_service_interface::param_item::ParamData::Structfield(
            vci_service_interface::ParamStructfield {
                data: Some(
                    vci_service_interface::param_structfield::Data::SessionTiming(
                        vci_service_interface::ParamSessionTimingList { entries: vec![] },
                    ),
                ),
            }
        )),
        "CP_SessionTimingOverride is allowed but unseeded on this preset (only the \
         ISO_15765_3 family seeds it) -- GetComParam must report an empty SessionTiming \
         Structfield, not Unum32(0)"
    );

    server.shutdown().await;
}

/// A2-18 Step D regression guard: a `SetComParam(Unum32(_))` write against a
/// Bytefield- or Structfield-typed ComParam must be rejected outright, not
/// silently accepted into `working.unum32` -- before this fix, only
/// `is_bustype_bytes_param` members (`CP_CanBaudrateRecord`) were rejected
/// this way (ADR-130); every other `BYTEFIELD_PARAMS`/`STRUCTFIELD_PARAMS`
/// member accepted a type-mismatched `Unum32` write with no error, which the
/// A2-18 `GetComParam` fallback fix above would then silently mask behind an
/// empty Bytefield/Structfield response instead of ever surfacing the
/// mismatch.
#[tokio::test]
#[serial]
async fn set_com_param_rejects_unum32_write_for_bytefield_and_structfield_params() {
    const CP_TESTER_PRESENT_MSG: u32 = 0x8001;
    const CP_EXTENDED_TIMING: u32 = 0x8050;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    for &com_param_id in &[CP_TESTER_PRESENT_MSG, CP_EXTENDED_TIMING] {
        let status = client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                    com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(1)),
                }),
            })
            .await
            .expect_err(&format!(
                "SetComParam({com_param_id:#x}, Unum32(_)) should be rejected -- this param is \
                 Bytefield/Structfield-typed"
            ));
        assert_eq!(
            status.code(),
            tonic::Code::InvalidArgument,
            "com_param_id {com_param_id:#x}"
        );
    }

    server.shutdown().await;
}

/// ISO 22900-2 §9.4.16.2.1 f): a ComParam of class `PDU_PC_TESTER_PRESENT`
/// may not be changed with the TempParamUpdate flag -- mirrors
/// `param_binding.rs`'s `bustype_guard_rejects_temp_param_update_for_sendrecv_and_startcomm`,
/// but staging `CP_TesterPresentHandling` (`0x8006`, `PDU_PC_TESTER_PRESENT`
/// class per `get_com_param_reports_correct_param_class` above) in Working
/// differently from Active instead of a BUSTYPE-class param. Closes the
/// backlog item ADR-133's Consequences section flagged as a residual
/// (Codex review finding on PR #147, `tester_present_params_differ`).
#[tokio::test]
#[serial]
async fn tester_present_guard_rejects_temp_param_update_for_sendrecv_and_startcomm() {
    const CP_TESTER_PRESENT_HANDLING: u32 = 0x8006;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // CP_TesterPresentHandling (TESTER_PRESENT class) differs between
    // Working (1, just staged) and Active (unset/0, never promoted by
    // CoptUpdateparam).
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;

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
        .expect_err(
            "CoptSendrecv temp_param_update should be rejected by the TESTER_PRESENT guard",
        );
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
        .expect_err(
            "CoptStartcomm temp_param_update should be rejected by the TESTER_PRESENT guard",
        );
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
        get_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING).await,
        1
    );

    server.shutdown().await;
}

/// ADR-133 amendment (Codex review round 2, PR #147): `CP_TesterPresentMessage`
/// unseeded on a raw (no-preset) CAN CLL reports an empty Bytefield via
/// `GetComParam` (A2-18). Writing that same empty Bytefield straight back
/// via `SetComParam` -- a completely normal read-then-write-back round-trip
/// a client performs to save/restore ComParam state -- must not
/// desynchronize Working from Active in a way `tester_present_params_differ`
/// treats as a real change. Before this fix, the explicit empty entry
/// landing in `working.bytes` compared unequal to Active's absent entry via
/// raw map presence, permanently rejecting every subsequent
/// `temp_param_update` COP with `PDU_ERR_TEMPPARAM_NOT_ALLOWED`, even one
/// staging a totally unrelated param (Codex's literal finding). Mirrors
/// `tester_present_guard_rejects_temp_param_update_for_sendrecv_and_startcomm`
/// above, with the assertion inverted: this round-trip must NOT be rejected.
#[tokio::test]
#[serial]
async fn tester_present_bytefield_empty_roundtrip_does_not_block_temp_param_update() {
    const CP_TESTER_PRESENT_MSG: u32 = 0x8001;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // CP_TesterPresentMessage is allowed for CAN but unseeded on a raw CAN
    // CLL -- GetComParam must report an empty Bytefield (A2-18).
    let item = get_com_param_item(&mut client, cll_handle, CP_TESTER_PRESENT_MSG).await;
    assert_eq!(
        item.param_data,
        Some(vci_service_interface::param_item::ParamData::Bytefield(
            Vec::new()
        )),
        "CP_TesterPresentMessage should be reported as an empty Bytefield when unseeded"
    );

    // Round-trip: write that same empty Bytefield straight back.
    set_com_param_bytes(&mut client, cll_handle, CP_TESTER_PRESENT_MSG, Vec::new()).await;

    // An unrelated temp_param_update COP must succeed -- the round-trip
    // above must not be treated as a TESTER_PRESENT-class change.
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
        .expect(
            "CoptSendrecv temp_param_update=1 must succeed -- a no-op Bytefield round-trip of \
             an unseeded TESTER_PRESENT-class param is not a real change",
        );

    server.shutdown().await;
}

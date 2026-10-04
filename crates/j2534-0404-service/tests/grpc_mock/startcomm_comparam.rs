//! ADR-067 (superseding ADR-066's live-at-execution design): `CoptStartcomm`
//! resolves the periodic tester-present message from the ACTIVE ComParam
//! snapshot bound at `StartComPrimitive` call time, and the transient init
//! transaction from Working when `temp_param_update` is set -- both COP
//! types honor `ComPrimitiveCtrlData.temp_param_update`, bounded (for
//! `CoptStartcomm`) to the transient init transaction only. `CoptStopcomm`
//! accepts the flag as a no-op. See ADR-067 for the full call-time-binding
//! model this supersedes ADR-063/064/066 with.

use serial_test::serial;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, ExpectedResponseData, LockResourceRequest, ModuleHandle,
    ParamItem, PinData, ResourceData, SetComParamRequest, StartComPrimitiveRequest,
    create_com_logical_link_request, event_item, param_item, pin_data, resource_data,
    subscribe_event_request,
};

use crate::harness::*;

/// Builds a `RscData` resource selecting `protocol_id` via the unambiguous
/// `ProtocolId` route with the given typed `(pin_number, pin_type_name)`
/// pairs as `dlc_pin_data` -- same per-file-local helper shape duplicated in
/// every test file that needs a raw `_PS`/pin-selection resource (see
/// `fd_can.rs`'s identical copy).
fn resource_with_protocol_id_and_pins(protocol_id: u32, pins: &[(u32, &str)]) -> ResourceData {
    ResourceData {
        dlc_pin_data: pins
            .iter()
            .map(|&(number, type_name)| PinData {
                dlc_pin_number: number,
                dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName(type_name.to_string())),
            })
            .collect(),
        bus_type: None,
        protocol: Some(resource_data::Protocol::ProtocolId(protocol_id)),
    }
}

/// Issues `CreateComLogicalLink` for `module_handle` with `resource`,
/// returning the raw `Result` so callers can assert either a handle or a
/// rejection -- same per-file-local helper shape as `fd_can.rs`'s/
/// `pin_selection.rs`'s identical copy.
async fn try_create_cll_with_resource(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    module_handle: u32,
    resource: ResourceData,
    _cll_tag: u64,
) -> Result<ComLogicalLinkHandle, tonic::Status> {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle { module_handle }),
            resource: Some(create_com_logical_link_request::Resource::RscData(resource)),
            cll_create_flag: None,
        })
        .await
        .map(|r| {
            r.into_inner()
                .cll_handle
                .expect("cll_handle should be present")
        })
}

/// ADR-067 (superseding ADR-066's FIFO-ordering guarantee): `CoptStartcomm`
/// resolves its tester-present message from the Active ComParam snapshot
/// bound at ITS OWN `StartComPrimitive` call time, not live at poll-task
/// execution time -- so whether an earlier `CoptUpdateparam` affects it
/// depends entirely on call ORDER, not FIFO enqueue order. This test makes
/// that deterministic by waiting for the `CoptUpdateparam`'s
/// `PduCopstFinished` event before issuing `CoptStartcomm` -- guaranteeing
/// the promotion to Active has already run by the time `CoptStartcomm`
/// binds its snapshot, so the update IS reflected: both the message content
/// and its functional addressing are staged in Working, promoted to Active
/// by `CoptUpdateparam`, and `CoptStartcomm`'s tester-present resolves
/// successfully and its arm-time send (ADR-084) goes out.
#[tokio::test]
#[serial]
async fn iso15765_queued_updateparam_is_reflected_by_startcomm_tester_present() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // Stage (Working only, no CoptUpdateparam yet): functional addressing
    // (so the tester-present message can be built with no UniqueRespIdTable
    // entry at all, ADR-054) plus the tester-present message/interval
    // themselves. Active is untouched by any of this.
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    // ADR-138: tester-present's own addressing ComParam, independent of
    // CP_RequestAddrMode above.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    // ADR-083: CP_TesterPresentTime is microsecond-resolution; 100_000 us
    // converts to a 100 ms native TimeInterval.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 100_000).await;
    // ADR-137: CP_TesterPresentHandling's ISO_15765_4 spec default is 0
    // (disabled); stage the promotion explicitly so tester-present still
    // arms -- this test is about the Active-snapshot-at-call-time mechanics,
    // not about CP_TesterPresentHandling itself.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Queue CoptUpdateparam -- promotes Working (tester-present + functional
    // addressing) to Active -- and wait for it to finish before issuing
    // CoptStartcomm: ADR-067 binds CoptStartcomm's Active snapshot at its
    // own call time, so only a call made AFTER CoptUpdateparam is known to
    // have completed is guaranteed to see its effect.
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
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptUpdateparam should finish"
    );

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

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish"
    );

    // CoptUpdateparam finished (and promoted Working to Active) strictly
    // before CoptStartcomm's call-time binding -- the message resolves
    // successfully and the arm-time send goes out.
    assert_eq!(server.backdoor.start_periodic_count(), 0);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    // Drop the still-open event subscription stream before shutting down --
    // otherwise the gRPC server's graceful shutdown waits on it indefinitely.
    drop(events);
    server.shutdown().await;
}

/// ADR-067 (superseding ADR-066): `CoptStartcomm` with `temp_param_update=1`
/// borrows the Working ComParam set (bound at call time) for its transient
/// init transaction only. The temp `SET_CONFIG` push (`effective`, the bound
/// Working snapshot) and the revert push (`revert_to`, the Active snapshot
/// bound at the SAME call time) are two distinct hardware writes bracketing
/// the init step; the tester-present arm-time send (sent only after the
/// revert) is Active-derived and unaffected by any of it; and (ADR-067 claim
/// D, superseding ADR-063/066's "Working is never promoted or reset") Working
/// is written back from Active as soon as the call returns -- a later
/// `CoptUpdateparam` finds Working already equal to Active, not the borrowed
/// value.
#[tokio::test]
#[serial]
async fn iso9141_temp_param_update_startcomm_borrows_working_for_init_then_reverts() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Create (but do not connect yet) so the tester-present message
    // (Bytefield, cannot go through create_and_connect_cll's Unum32-only
    // helper) can be staged before ConnectComLogicalLink promotes the first
    // CLL's Working set to Active. CP_TesterPresentMessage's addressing (the
    // ISO9141 KWP2000 header) needs no UniqueRespIdTable entry at all --
    // falls back to this service's documented default address bytes.
    let cll_handle = create_cll(&mut client, j2534_0404::ISO9141, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    // ADR-072: ComParam-space P1_MAX is microseconds; 10_000 us converts to
    // native P1_MAX = 20 (0.5 ms steps), matching this test's pre-ADR-072
    // "P1_MAX=20" native value throughout.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P1_MAX, 10_000).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    // ADR-083: CP_TesterPresentTime is microsecond-resolution; 100_000 us
    // converts to a 100 ms native TimeInterval.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 100_000).await;
    // ADR-137: create_cll's bare protocol id (no resource/bustype match)
    // seeds no ComParam defaults at all, so CP_TesterPresentHandling has no
    // seeded value here -- unlike a real ISO9141 preset (whose spec default
    // is 1) -- and must be set explicitly for tester-present to arm.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // Active now has P1_MAX=20 (pushed to hardware at connect).
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P1_MAX),
        20
    );
    let baseline_set_config = server.backdoor.set_config_count();

    // Stage (Working only, no CoptUpdateparam) a different P1_MAX -- Active
    // stays 20 until CoptUpdateparam is called explicitly. 49_500 us
    // converts to native P1_MAX = 99 (0.5 ms steps, ADR-072), distinct from
    // the 20 staged above so the borrow/revert below is observable.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P1_MAX, 49_500).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // ISO9141 always uses five-baud init (deterministic in the mock,
    // IOCTL_FIVE_BAUD_INIT always succeeds); a single address byte triggers it.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x33],
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
        .expect("start_com_primitive(CoptStartcomm, temp_param_update=1) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish"
    );

    // Two SET_CONFIG calls bracket the init step: apply Working (P1_MAX=99),
    // then revert to Active (P1_MAX=20) before the tester-present arm-time
    // send is ever attempted -- not zero (an ordinary CoptStartcomm, source ==
    // Active, performs neither) and not one (which would mean either the
    // apply or the revert was skipped).
    assert_eq!(server.backdoor.set_config_count(), baseline_set_config + 2);
    // Hardware is left at Active, not stuck on the borrowed Working value.
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P1_MAX),
        20
    );
    // The tester-present arm-time send went out -- resolved from Active
    // (unaffected by which set P1_MAX's init transaction borrowed).
    assert_eq!(server.backdoor.start_periodic_count(), 0);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    // ADR-067 claim D (superseding ADR-063/066's "Working is never reset"):
    // Working was written back from Active as soon as the temp_param_update
    // call RETURNED (synchronously, in rpc_start_com_primitive, before the
    // COP's own hardware apply/revert even ran) -- GetComParam now shows
    // Working == 10_000 (the ComParam-space microsecond value that
    // originally forwarded as native P1_MAX = 20), not the borrowed 49_500.
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::P1_MAX).await,
        10_000
    );

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStopcomm should finish"
    );

    // A subsequent CoptUpdateparam is now a no-op with respect to P1_MAX --
    // Working already equals Active (20) since the writeback above, so it
    // stays 20 rather than promoting the borrowed 99.
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
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P1_MAX),
        20
    );
    assert_eq!(server.backdoor.set_config_count(), baseline_set_config + 3);

    // Drop the still-open event subscription stream before shutting down --
    // otherwise the gRPC server's graceful shutdown waits on it indefinitely.
    drop(events);
    server.shutdown().await;
}

/// ADR-110 (superseding ADR-044/ADR-066's synchronous rejection, per the
/// amendment note added to ADR-067 §H): `CoptStartcomm` with
/// `temp_param_update=1` pushes the borrowed Working set to hardware for the
/// duration of its init transaction (`apply_params_to_hardware`), same as
/// `CoptSendrecv`+`temp_param_update` and `CoptUpdateparam` -- but is no
/// longer blocked by another CLL's `LOCK_PHYSICAL_COM_PARAMS` at all;
/// ADR-067 §E's own `PDU_ERR_TEMPPARAM_NOT_ALLOWED` BUSTYPE-differ guard is
/// the sole call-time physical-ComParam gate left for `temp_param_update`,
/// extending the existing
/// `start_com_primitive_temp_param_update_is_not_gated_by_physical_com_param_lock`
/// (`CoptSendrecv`) coverage to `CoptStartcomm`. `CoptStartcomm` without
/// `temp_param_update` writes no hardware config at all and was never
/// affected by this lock either.
#[tokio::test]
#[serial]
async fn start_com_primitive_startcomm_temp_param_update_is_not_gated_by_physical_com_param_lock() {
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

    // cll_b joins cll_a's already-open physical channel, so its Active stays
    // default (empty) until it issues CoptUpdateparam (a pre-existing,
    // separately-documented quirk -- see ADR-060's Consequences). Sync it
    // here so ADR-067 §E's BUSTYPE guard (which compares Working vs Active
    // on CP_Baudrate, among others) does not itself reject the
    // temp_param_update call below -- isolating this test to the
    // LOCK_PHYSICAL_COM_PARAMS question alone.
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
        .expect(
            "CoptStartcomm with temp_param_update should succeed even when another CLL holds \
             the physical ComParam lock (ADR-110)",
        );
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Close comm before the next CoptStartcomm -- the call above actually
    // started communication now that it is no longer synchronously rejected.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // CoptStartcomm without temp_param_update writes no hardware ComParam
    // config at all and is therefore not gated by LOCK_PHYSICAL_COM_PARAMS
    // either -- unaffected by this ADR, still verified here for completeness.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("CoptStartcomm without temp_param_update should not be blocked by the physical ComParam lock");

    server.shutdown().await;
}

/// Regression test for a confirmed design flaw in this ADR's first cut
/// (edge-case-hunter repro, corrected by `comparam_support::strip_bustype_keys`):
/// ISO 22900-2 §9.4.16.2.1 c) NOTE 2 / d) says a `PDU_PC_BUSTYPE`-class
/// ComParam can never be changed via `TempParamUpdate` at all -- lock or no
/// lock -- because the temp bracket applies/reverts against THIS CLL's own
/// `Active`, which has no cross-CLL sync and can already be stale relative to
/// another CLL's real, currently-pushed hardware value, with no lock
/// required to expose the gap. `cll_b` syncs Working == Active once
/// (unlocked), `cll_a` then changes the real hardware value with NO lock
/// held anywhere, and `cll_b`'s own Working/Active never change again -- so
/// a `temp_param_update=1` `CoptStartcomm` from `cll_b` must not push its own
/// stale `CP_BitSamplePoint` to hardware (at apply OR at revert time),
/// clobbering `cll_a`'s real, current value.
#[tokio::test]
#[serial]
async fn start_com_primitive_startcomm_temp_param_update_does_not_clobber_hardware_with_stale_own_active()
 {
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
    // it once, unlocked, so its own Working == its own Active afterward, and
    // ADR-067 §E's BUSTYPE-differ guard will not itself reject the temp call
    // below.
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
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BIT_SAMPLE_POINT),
        80
    );

    // cll_a changes the real hardware value -- with NO lock held anywhere --
    // so cll_b's own Active is now stale relative to hardware, with no local
    // signal of that.
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_a),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    j2534_0404::BIT_SAMPLE_POINT,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(60)),
            }),
        })
        .await
        .expect("set_com_param should succeed");
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: Vec::new(),
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptUpdateparam) should succeed for cll_a");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BIT_SAMPLE_POINT),
        60,
        "cll_a's own CoptUpdateparam must have actually pushed the new value to hardware"
    );

    // No lock held anywhere. cll_b's own Working still equals its own
    // (stale) Active -- 80 == 80 -- so ADR-067 §E's guard passes, but the
    // temp apply/revert bracket must not clobber cll_a's real hardware value
    // of 60 with cll_b's stale 80.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
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
        .expect("CoptStartcomm with temp_param_update should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::BIT_SAMPLE_POINT),
        60,
        "cll_b's stale own-Active value (80) must NOT have clobbered cll_a's real hardware \
         value (60) at apply OR at revert time -- the confirmed regression this test pins"
    );

    server.shutdown().await;
}

/// ADR-066: `CoptStopcomm` accepts `temp_param_update=1` as a no-op --
/// `handle_stop_comm` reads no ComParams and writes no hardware config
/// regardless of the flag, so it behaves identically to the flag unset (no
/// extra `SET_CONFIG` call, and it is not gated by `LOCK_PHYSICAL_COM_PARAMS`
/// -- unlike `CoptStartcomm`/`CoptSendrecv` with the same flag).
#[tokio::test]
#[serial]
async fn stopcomm_temp_param_update_is_accepted_as_a_no_op() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id(&mut client, cll_handle, 0x7E0).await;

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish"
    );

    let baseline_set_config = server.backdoor.set_config_count();

    // CoptStopcomm with temp_param_update=1 must succeed and behave exactly
    // like the flag unset: no SET_CONFIG call, since handle_stop_comm never
    // touches hardware ComParam config.
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
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStopcomm should finish"
    );

    assert_eq!(server.backdoor.set_config_count(), baseline_set_config);

    // Drop the still-open event subscription stream before shutting down --
    // otherwise the gRPC server's graceful shutdown waits on it indefinitely.
    drop(events);
    server.shutdown().await;
}

/// ADR-076: an explicit `CP_InitializationSettings=1` is the spec-mandated
/// 5-baud contract, which allows NO optional message at all -- unlike the
/// pre-ADR-076 behavior (`CP_InitializationSettings=1` simply overriding the
/// init-sequence choice while still sending `init_data[0]` as the address),
/// any non-empty `cop_data` is now a synchronous `INVALID_ARGUMENT`, and the
/// COP never reaches the adapter.
#[tokio::test]
#[serial]
async fn iso14230_init_settings_five_baud_rejects_non_empty_cop_data() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Stage CP_InitializationSettings in Working *before* connecting, so
    // ConnectComLogicalLink's Working -> Active promotion carries it into
    // the Active snapshot `CoptStartcomm` (without temp_param_update) binds
    // from -- setting it after connect would only reach Working, never Active.
    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x68, 0x6A, 0xF1, 0x81],
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "a 5-baud initialization start communication request must not carry an optional message",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    assert_eq!(
        server.backdoor.five_baud_init_count(),
        0,
        "the rejected COP must never reach the adapter"
    );
    assert_eq!(server.backdoor.fast_init_count(), 0);

    server.shutdown().await;
}

/// ADR-074: `CP_InitializationSettings=2` selects fast-init regardless of
/// `init_data`'s length -- the pre-ADR-074 heuristic would have picked
/// five-baud init here (a single address byte).
#[tokio::test]
#[serial]
async fn iso14230_init_settings_fast_overrides_single_byte_init_data() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // See the comment in the five-baud override test above: the param must
    // be staged before connect to reach the Active snapshot.
    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 2).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x33],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish"
    );

    assert_eq!(
        server.backdoor.fast_init_count(),
        1,
        "CP_InitializationSettings=2 should force fast-init"
    );
    assert_eq!(server.backdoor.five_baud_init_count(), 0);

    drop(events);
    server.shutdown().await;
}

/// ADR-075: an oversized fast-init payload is rejected synchronously with
/// INVALID_ARGUMENT -- a 256-byte payload cannot be encoded in the single
/// KWP length byte (and its 260-byte frame would also breach ISO14230's
/// 259-byte SAE J2534-1 TX ceiling); without the call-time rejection it
/// would wrap into a malformed async send.
#[tokio::test]
#[serial]
async fn iso14230_fast_init_rejects_oversized_payload_synchronously() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 2).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x81; 256],
            cop_ctrl_data: None,
        })
        .await
        .expect_err("an oversized fast-init payload should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("KWP length byte"),
        "unexpected error message: {}",
        status.message()
    );

    assert_eq!(
        server.backdoor.fast_init_count(),
        0,
        "the rejected COP must never reach the adapter"
    );

    server.shutdown().await;
}

/// ADR-075: ISO9141's SAE J2534-1 frame ceiling is 4128 bytes, so the
/// frame-size check alone cannot catch a payload the single KWP length byte
/// cannot encode -- a 300-byte fast-init payload must still be rejected
/// synchronously instead of wrapping the length byte on the wire.
#[tokio::test]
#[serial]
async fn iso9141_fast_init_rejects_payload_beyond_kwp_length_byte() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO9141, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 2).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x81; 300],
            cop_ctrl_data: None,
        })
        .await
        .expect_err("a payload beyond the KWP length byte should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("KWP length byte"),
        "unexpected error message: {}",
        status.message()
    );

    assert_eq!(
        server.backdoor.fast_init_count(),
        0,
        "the rejected COP must never reach the adapter"
    );

    server.shutdown().await;
}

/// ADR-076: the pre-ADR-074 legacy heuristic path (`CP_InitializationSettings`
/// absent from the bound set) is unchanged by ADR-076's spec 5-baud contract
/// -- unlike an explicit `CP_InitializationSettings=1`, it still uses the raw
/// `cop_data[0]` byte as the 5-baud target address, still delivers keybytes
/// unconditionally, and does not reject a non-empty `cop_data`.
/// `create_cll` with a bare protocol id (no resource/bustype match) seeds no
/// ComParam defaults at all, so `CP_InitializationSettings` never gets set
/// here -- ISO9141 always selects five-baud under the legacy heuristic
/// regardless.
///
/// Also covers the ADR-076 `CP_Baudrate` write-back on the legacy path: after
/// a successful 5-baud init, `GetComParam(CP_Baudrate)` reflects the baud
/// rate the mock simulates as "negotiated" during the sequence.
#[tokio::test]
#[serial]
async fn iso9141_legacy_heuristic_uses_raw_cop_data_byte_as_address() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO9141, 1).await;
    // Distinct from `MOCK_FIVE_BAUD_NEGOTIATED_BAUD` so the write-back below
    // is observable.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 9_600).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x42],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    // ADR-115 single-consumer correction: a live subscriber is attached
    // above, so both PduCopstFinished and this response's ResultData are
    // delivered on the live stream instead of GetEventItem --
    // `wait_for_cop_finished_and_result_data` captures both without
    // assuming which arrives first (see its own doc comment).
    let result = wait_for_cop_finished_and_result_data(&mut events, 2000).await;

    assert_eq!(server.backdoor.five_baud_init_count(), 1);
    assert_eq!(server.backdoor.fast_init_count(), 0);
    assert_eq!(
        server.backdoor.five_baud_init_input(MOCK_CHANNEL_ID),
        Some(0x42),
        "the legacy path must use the raw cop_data[0] byte as the 5-baud address"
    );

    // ADR-075: five-baud keybytes are delivered raw, unlike fast-init's
    // header/payload split.
    assert_result_data(&result, &[], &[], &[0x55, 0x8F]);

    // ADR-076: CP_Baudrate write-back also happens on the legacy path.
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE).await,
        MOCK_FIVE_BAUD_NEGOTIATED_BAUD
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-074: `CP_InitializationSettings=3` skips the init call entirely --
/// neither `IOCTL_FIVE_BAUD_INIT` nor `IOCTL_FAST_INIT` is issued -- and the
/// COP still proceeds and finishes normally, as though init had succeeded.
#[tokio::test]
#[serial]
async fn iso14230_init_settings_none_skips_init_sequence() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // See the comment in the five-baud override test above: the param must
    // be staged before connect to reach the Active snapshot.
    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 3).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x33],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed even with CP_InitializationSettings=3");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish even though the init sequence was skipped"
    );

    assert_eq!(
        server.backdoor.five_baud_init_count(),
        0,
        "CP_InitializationSettings=3 must not issue a five-baud init"
    );
    assert_eq!(
        server.backdoor.fast_init_count(),
        0,
        "CP_InitializationSettings=3 must not issue a fast-init"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-074: `SetComParam(CP_InitializationSettings, ...)` only accepts `1`
/// (5-baud init), `2` (fast-init), or `3` (no init); anything else is
/// rejected with `INVALID_ARGUMENT` rather than silently falling back to the
/// legacy heuristic at `CoptStartcomm` time.
#[tokio::test]
#[serial]
async fn set_com_param_init_settings_rejects_out_of_range_values() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;

    for &value in &[0u32, 4u32] {
        let status = client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(
                        CP_INIT_SETTINGS,
                    )),
                    com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(value)),
                }),
            })
            .await
            .expect_err(&format!(
                "SetComParam(CP_InitializationSettings={value}) should be rejected"
            ));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    for &value in &[1u32, 2u32, 3u32] {
        client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(
                        CP_INIT_SETTINGS,
                    )),
                    com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(value)),
                }),
            })
            .await
            .unwrap_or_else(|_| {
                panic!("SetComParam(CP_InitializationSettings={value}) should be accepted")
            });
    }

    server.shutdown().await;
}

/// Resource `0x0213` (`ISO_OBD_on_K_Line`) seeds `CP_InitializationSettings
/// = 2` (fast-init) by default -- its connect protocol is fixed to
/// `ISO9141` (ADR-069), and per J2534-1 v04.04 FAST_INIT is valid on
/// ISO9141 as well as ISO14230, so the seeded default selects fast-init
/// with no client-side ComParam override at all.
#[tokio::test]
#[serial]
async fn resource_0213_default_path_selects_fast_init_with_no_overrides() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Create by resource id (0x0213 resolves through the resources table,
    // per `create_com_logical_link_resolves_table_resource_id_to_expected_j2534_protocol`
    // in resources.rs) with NO ComParam overrides at all -- purely the
    // `iso_obd_on_k_line` protocol-default preset plus the
    // `ISO_9141_2_UART_and_ISO_14230_1_UART` bustype default.
    let cll_handle = create_and_connect_cll(&mut client, 0x0213, &[]).await;

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x33],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish"
    );

    assert_eq!(
        server.backdoor.fast_init_count(),
        1,
        "resource 0x0213's default (CP_InitializationSettings=2) should select fast-init \
         with no client overrides -- valid on its fixed ISO9141 connect protocol"
    );
    assert_eq!(server.backdoor.five_baud_init_count(), 0);

    drop(events);
    server.shutdown().await;
}

/// An explicit `CP_InitializationSettings=2` on an ISO9141 link is forwarded
/// to the adapter as requested -- no silent reroute to 5-baud. Per J2534-1
/// v04.04, FAST_INIT is valid on both K-line protocols (ISO9141 and
/// ISO14230), so this succeeds like any other `CoptStartcomm`.
///
/// ADR-075: `cop_data` is payload-only for fast-init -- the KWP wakeup
/// header is constructed by this service, at `StartComPrimitive` call time,
/// from ComParams/the UniqueRespIdTable (all defaults here: format `0x80`,
/// target `0x10`, source `0xF1`, one payload byte). This test asserts both
/// the exact input frame the mock's `IOCTL_FAST_INIT` recorded and the
/// header/payload split of the canned `MOCK_FAST_INIT_RESPONSE` the service
/// delivers back.
#[tokio::test]
#[serial]
async fn iso9141_explicit_fast_init_setting_succeeds() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Stage CP_InitializationSettings=2 in Working before connect, same as
    // the other explicit-override tests above, so it reaches the Active
    // snapshot CoptStartcomm (without temp_param_update) binds from.
    let cll_handle = create_cll(&mut client, j2534_0404::ISO9141, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 2).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x81],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    // ADR-115 single-consumer correction: a live subscriber is attached
    // above, so both PduCopstFinished and this response's ResultData are
    // delivered on the live stream instead of GetEventItem --
    // `wait_for_cop_finished_and_result_data` captures both without
    // assuming which arrives first (see its own doc comment).
    let result = wait_for_cop_finished_and_result_data(&mut events, 2000).await;

    assert_eq!(
        server.backdoor.fast_init_count(),
        1,
        "CP_InitializationSettings=2 should force fast-init, which succeeds on ISO9141"
    );
    assert_eq!(server.backdoor.five_baud_init_count(), 0);

    // ADR-075: the recorded IOCTL_FAST_INIT input is the payload prefixed
    // with the default-ComParam KWP header (format 0x80, target 0x10,
    // source 0xF1, embedded length 0x01).
    assert_eq!(
        server.backdoor.fast_init_input(MOCK_CHANNEL_ID),
        Some(vec![0x80, 0x10, 0xF1, 0x01, 0x81]),
        "the KWP wakeup header should be built from default ComParams at call time"
    );

    // ADR-075: the canned MOCK_FAST_INIT_RESPONSE ([0x83, 0xF1, 0x10, 0xC1,
    // 0xE9, 0x8F]) is delivered with its 3-byte KWP header split into
    // extra_info and the 3-byte payload in data_bytes, no footer.
    assert_result_data(&result, &[0x83, 0xF1, 0x10], &[], &[0xC1, 0xE9, 0x8F]);

    // ADR-143: the synthetic fast-init response frame's rx_status_flags is
    // always 0 (no real RxStatus indication was captured for it), so both
    // timestamp fields are naturally None -- correct per the flag-gated-
    // validity semantics, not a gap.
    assert_eq!(result.tx_msg_done_timestamp, None);
    assert_eq!(result.start_msg_timestamp, None);

    drop(events);
    server.shutdown().await;
}

/// ADR-077: the D-PDU API spec makes the fast-init service request after the
/// wakeup pattern OPTIONAL. An explicit `CP_InitializationSettings=2` with
/// EMPTY `cop_data` on a K-line link sends only the wakeup pattern --
/// `PassThruIoctl(FAST_INIT)` is called with a NULL input message (no KWP
/// header is built), the init count still increments, and -- because no
/// request was sent -- no response is delivered at all: neither a
/// `ReceivedFrame` in the CLL's receive buffer nor a `ResultData`
/// `SubscribeEvent` notification. `PduCllstCommStarted` and `PduCopstFinished`
/// are still emitted, same as any other successful `CoptStartcomm`.
#[tokio::test]
#[serial]
async fn iso14230_init_settings_fast_wakeup_only_on_empty_cop_data() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // See the comment in the five-baud override test above: the param must
    // be staged before connect to reach the Active snapshot.
    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 2).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    // Drain the SubscribeEvent stream in a single pass, tracking both
    // `PduCllstCommStarted` and any `ResultData` notification along the way,
    // until `PduCopstFinished` arrives -- `wait_for_event`'s predicate-match
    // loop silently skips non-matching frames when scanning for a single
    // event, so two separate `wait_for_event` calls (one per event type)
    // would not by themselves prove a `ResultData` never appeared in
    // between. This directly exercises the "no ResultData/rx frame
    // delivered for the COP" claim on the fan-out path (`assert_no_result_data`
    // below only covers the separate `GetEventItem`/rx_buf path).
    let mut saw_cll_started = false;
    let mut saw_result_data = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            match &item.data {
                Some(event_item::Data::CllStatus(status))
                    if *status
                        == vci_service_interface::PduComLogicalLinkStatus::PduCllstCommStarted
                            as i32 =>
                {
                    saw_cll_started = true;
                }
                Some(event_item::Data::ResultData(_)) => {
                    saw_result_data = true;
                }
                _ => {}
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "CoptStartcomm should finish"
    );
    assert!(
        saw_cll_started,
        "wakeup-only fast-init should still emit PduCllstCommStarted"
    );
    assert!(
        !saw_result_data,
        "a wakeup-only fast-init sends no request, so no ResultData notification should be \
         delivered on the SubscribeEvent stream before PduCopstFinished"
    );

    assert_eq!(
        server.backdoor.fast_init_count(),
        1,
        "the wakeup pattern should still be sent"
    );
    assert_eq!(server.backdoor.five_baud_init_count(), 0);
    assert_eq!(
        server.backdoor.fast_init_input(MOCK_CHANNEL_ID),
        None,
        "no service request should follow the wakeup pattern -- FAST_INIT is called with a NULL input"
    );

    assert_no_result_data(
        &mut client,
        cll_handle,
        "a wakeup-only fast-init sends no request, so no response should be delivered",
    )
    .await;

    drop(events);
    server.shutdown().await;
}

/// ADR-077: the wakeup-only fast-init path is gated on the EXPLICIT
/// `CP_InitializationSettings=2` value, not on `select_init_sequence`'s
/// result -- the legacy heuristic (param unset) also resolves to `Fast` for
/// empty `init_data` on ISO14230, but empty `cop_data` on that path must keep
/// meaning "skip init entirely", exactly as before ADR-077. `create_cll` with
/// a bare protocol id seeds no ComParam defaults, so
/// `CP_InitializationSettings` stays unset here.
#[tokio::test]
#[serial]
async fn iso14230_legacy_heuristic_empty_cop_data_still_skips_init() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed even though init is skipped");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish even though the init sequence was skipped"
    );

    assert_eq!(
        server.backdoor.fast_init_count(),
        0,
        "the legacy heuristic's empty-cop_data behavior must stay 'skip init', unaffected by ADR-077"
    );
    assert_eq!(server.backdoor.five_baud_init_count(), 0);

    drop(events);
    server.shutdown().await;
}

/// ADR-077: an explicit `CP_InitializationSettings=2` with empty `cop_data`
/// fires wakeup-only fast-init identically on ISO9141 -- per J2534-1 v04.04,
/// `FAST_INIT` is valid on both K-line protocols, so there is no
/// protocol-specific special-casing (mirrors
/// `iso9141_explicit_fast_init_setting_succeeds`'s "no silent reroute"
/// stance for the `WithRequest` case).
#[tokio::test]
#[serial]
async fn iso9141_init_settings_fast_wakeup_only_on_empty_cop_data() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO9141, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 2).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish -- FAST_INIT is valid on ISO9141 per J2534-1 v04.04"
    );

    assert_eq!(
        server.backdoor.fast_init_count(),
        1,
        "wakeup-only fast-init should fire on ISO9141 identically to ISO14230"
    );
    assert_eq!(server.backdoor.five_baud_init_count(), 0);
    assert_eq!(
        server.backdoor.fast_init_input(MOCK_CHANNEL_ID),
        None,
        "no service request should follow the wakeup pattern -- FAST_INIT is called with a NULL input"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-077: `temp_param_update=1` borrows the Working ComParam snapshot for
/// the init transaction only -- so the wakeup-only fast-init gate
/// (`CP_InitializationSettings == 2`, explicit) must resolve from
/// `binding.resolved()` (Working, not Active) when the two disagree. Mirrors
/// `iso14230_temp_param_update_startcomm_resolves_five_baud_address_from_working`'s
/// structure: Active ends up with an explicit `CP_InitializationSettings=3`
/// (skip init) after connect, distinct from Working's `2` (wakeup-only fast
/// init) staged below, so a wrong (Active) resolution would visibly skip the
/// init entirely instead of running it.
#[tokio::test]
#[serial]
async fn iso14230_temp_param_update_startcomm_resolves_wakeup_only_fast_init_from_working() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 3).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // Stage (Working only, no CoptUpdateparam) the wakeup-only setting --
    // Active stays at CP_InitializationSettings=3 (skip) until
    // CoptUpdateparam is called explicitly.
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 2).await;

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
        .expect("start_com_primitive(CoptStartcomm, temp_param_update=1) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish"
    );

    assert_eq!(
        server.backdoor.fast_init_count(),
        1,
        "temp_param_update=1 should bind the wakeup-only gate's \
         CP_InitializationSettings from Working (2), not Active (3) -- \
         Active's value would have skipped the init entirely"
    );
    assert_eq!(server.backdoor.five_baud_init_count(), 0);
    assert_eq!(
        server.backdoor.fast_init_input(MOCK_CHANNEL_ID),
        None,
        "the wakeup-only fast-init should still run with a NULL input"
    );

    // ADR-067 claim D: Working was written back from Active as soon as the
    // temp_param_update call returned -- GetComParam now shows Active's
    // original CP_InitializationSettings=3, not the borrowed 2.
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS).await,
        3
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-077: when the wakeup-only fast-init ioctl itself fails, `CoptStartcomm`
/// follows the same init-error path any other init failure does
/// (`events.rs`'s `run_protocol_init` `Err` arm): a `temp_param_update=1`
/// transaction is reverted to the live Active set before the failure is
/// surfaced, `PduErrEvtInitError` and `PduCopstFinished` are both emitted,
/// and -- since the failure returns early, before `LogicalLinkState.comm_started`
/// is ever set -- the CLL is left exactly as if `CoptStartcomm` had never
/// been attempted: a subsequent `CoptStartcomm` call is still accepted (not
/// rejected as "already started") and can succeed once the fault clears,
/// demonstrating nothing is left locked/stuck by the failed attempt.
#[tokio::test]
#[serial]
async fn iso14230_wakeup_only_fast_init_failure_emits_error_and_reverts_temp_params() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // `P1_MAX` (a real J2534 `SET_CONFIG` parameter, ADR-072) is staged
    // alongside `CP_InitializationSettings` so `apply_params_to_hardware`/
    // `revert_hardware_to_live_active` below actually have something to push
    // -- `CP_InitializationSettings` and `DATA_RATE` alone are both filtered
    // out of every `SET_CONFIG` push (the former has no J2534 CONFIG_ID
    // equivalent at all; the latter is fixed at connect time, ADR-011), so a
    // CLL staged with only those two would see zero `SET_CONFIG` calls
    // either way and could not distinguish "the revert ran" from "there was
    // nothing to revert".
    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P1_MAX, 10_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 2).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let baseline_set_config = server.backdoor.set_config_count();

    server
        .backdoor
        .set_fast_init_error(Some(j2534_0404::ERR_FAILED as std::os::raw::c_long));

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
        .expect("start_com_primitive(CoptStartcomm, temp_param_update=1) should succeed -- the \
                 ioctl failure surfaces asynchronously as PduErrEvtInitError, not a synchronous error");

    let mut saw_init_error = false;
    assert!(
        wait_for_event(&mut events, 2000, |item| {
            if matches!(
                &item.data,
                Some(event_item::Data::ErrorData(error))
                    if *error == vci_service_interface::PduErrorEvent::PduErrEvtInitError as i32
            ) {
                saw_init_error = true;
            }
            matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
            )
        })
        .await,
        "the failed CoptStartcomm should still finish (PduCopstFinished), not hang"
    );
    assert!(
        saw_init_error,
        "a failed wakeup-only fast-init should emit PduErrEvtInitError"
    );

    // The temp_param_update=1 transaction brackets the (failed) init step
    // with exactly two SET_CONFIG calls: apply Working, then revert to the
    // live Active set on failure (ADR-067) -- mirrors
    // `iso9141_temp_param_update_startcomm_borrows_working_for_init_then_reverts`'s
    // successful-path count, proving the revert also runs on this failure path.
    assert_eq!(server.backdoor.set_config_count(), baseline_set_config + 2);

    // The forced ioctl failure is checked before the mock's success counter
    // increments (mirrors every other rejected-call counter convention in
    // this mock), so the failed attempt is not counted as a successful
    // fast-init.
    assert_eq!(server.backdoor.fast_init_count(), 0);

    // Nothing is left locked/stuck by the failed attempt: `comm_started`
    // was never set (the error path returns before Step 3), so a second
    // CoptStartcomm is still accepted, and now succeeds once the fault
    // clears.
    server.backdoor.set_fast_init_error(None);
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "a retried CoptStartcomm should be accepted -- the earlier failure left nothing locked",
        );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "the retried CoptStartcomm should finish successfully"
    );
    assert_eq!(
        server.backdoor.fast_init_count(),
        1,
        "the retry should run the wakeup-only fast-init to completion"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-067 + ADR-076: `temp_param_update=1` borrows the Working ComParam
/// snapshot for the init transaction only -- so the spec 5-baud contract
/// (`CP_InitializationSettings == 1`) must resolve its target address from
/// Working, not Active, when Working and Active disagree. Mirrors
/// `iso9141_temp_param_update_startcomm_borrows_working_for_init_then_reverts`'s
/// structure, and also covers ADR-067 claim D (Working is written back from
/// Active as soon as the call returns) and the ADR-076 `CP_Baudrate`
/// write-back together.
#[tokio::test]
#[serial]
async fn iso14230_temp_param_update_startcomm_resolves_five_baud_address_from_working() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Active ends up with CP_InitializationSettings=2 (fast-init) after
    // connect -- distinct from Working's 1 (five-baud) staged below, so a
    // wrong (Active) resolution would visibly select the wrong sequence.
    // Distinct DATA_RATE from `MOCK_FIVE_BAUD_NEGOTIATED_BAUD` so the
    // write-back below is observable.
    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 9_600).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 2).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // Stage (Working only, no CoptUpdateparam) the spec 5-baud contract and a
    // distinctive CP_5BaudAddressPhys -- Active stays at
    // CP_InitializationSettings=2 / the default CP_5BaudAddressPhys (0x01)
    // until CoptUpdateparam is called explicitly.
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_5BAUD_ADDR_PHYS, 0x55).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // The spec 5-baud contract allows no optional message -- cop_data must
    // be empty regardless of temp_param_update.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 1,
                temp_param_update: 1,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStartcomm, temp_param_update=1) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish"
    );

    assert_eq!(
        server.backdoor.five_baud_init_count(),
        1,
        "temp_param_update=1 should bind the init transaction's \
         CP_InitializationSettings from Working (1), not Active (2)"
    );
    assert_eq!(server.backdoor.fast_init_count(), 0);
    assert_eq!(
        server.backdoor.five_baud_init_input(MOCK_CHANNEL_ID),
        Some(0x55),
        "the 5-baud address should be resolved from the bound Working snapshot \
         (CP_5BaudAddressPhys=0x55), not the Active snapshot's default 0x01"
    );

    // ADR-067 claim D: Working was written back from Active as soon as the
    // temp_param_update call returned -- GetComParam now shows Active's
    // original CP_InitializationSettings=2, not the borrowed 1.
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS).await,
        2
    );
    // ADR-076: CP_Baudrate write-back happens regardless of temp_param_update
    // -- both Working and Active reflect the negotiated rate.
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE).await,
        MOCK_FIVE_BAUD_NEGOTIATED_BAUD
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-074: `CP_InitializationSettings=3` skips the init call, but the COP
/// otherwise proceeds exactly like an ordinary successful `CoptStartcomm` --
/// in particular the tester-present arm-time send (when configured) still
/// goes out.
#[tokio::test]
#[serial]
async fn iso14230_init_settings_none_still_starts_tester_present() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Create (but do not connect yet) so the Bytefield tester-present
    // message can be staged before ConnectComLogicalLink promotes Working
    // to Active -- same pattern as the ISO9141 temp_param_update test above.
    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 3).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    // ADR-083: CP_TesterPresentTime is microsecond-resolution; 100_000 us
    // converts to a 100 ms native TimeInterval.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 100_000).await;
    // ADR-137: create_cll's bare protocol id (no resource/bustype match)
    // seeds no ComParam defaults at all, so CP_TesterPresentHandling has no
    // seeded value here -- unlike a real ISO14230 preset (whose spec default
    // is 1) -- and must be set explicitly for tester-present to arm.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x33],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed even with CP_InitializationSettings=3");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish even though the init sequence was skipped"
    );

    assert_eq!(server.backdoor.five_baud_init_count(), 0);
    assert_eq!(server.backdoor.fast_init_count(), 0);
    assert_eq!(server.backdoor.start_periodic_count(), 0);
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the tester-present arm-time send should still go out when init is skipped"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-076: the spec-mandated 5-baud contract's happy path -- explicit
/// `CP_InitializationSettings=1`, empty `cop_data`, `NumReceiveCycles=1`. The
/// target address is sourced from `CP_5BaudAddressPhys` (staged to a
/// distinctive value via `SetComParam` + `CoptUpdateparam`, proving the
/// address comes from ComParams, not `cop_data`), the ECU key bytes are
/// delivered raw via `ResultData`, `CP_Baudrate` reflects the negotiated
/// rate afterward, and the tester-present arm-time send still goes out.
#[tokio::test]
#[serial]
async fn iso14230_spec_five_baud_contract_happy_path() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    // Distinct from `MOCK_FIVE_BAUD_NEGOTIATED_BAUD` so the write-back below
    // is observable.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 9_600).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    // ADR-083: CP_TesterPresentTime is microsecond-resolution; 100_000 us
    // converts to a 100 ms native TimeInterval.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 100_000).await;
    // ADR-137: create_cll's bare protocol id (no resource/bustype match)
    // seeds no ComParam defaults at all, so CP_TesterPresentHandling has no
    // seeded value here -- unlike a real ISO14230 preset (whose spec default
    // is 1) -- and must be set explicitly for tester-present to arm.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // Stage a distinctive CP_5BaudAddressPhys and promote it to Active --
    // proves the spec path sources the address from bound ComParams, not
    // `cop_data` (which must be empty on this path anyway).
    set_com_param_unum32(&mut client, cll_handle, CP_5BAUD_ADDR_PHYS, 0x42).await;
    promote_via_update_param(&mut client, cll_handle).await;

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
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
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    // ADR-115 single-consumer correction: a live subscriber is attached
    // above, so both PduCopstFinished and this response's ResultData are
    // delivered on the live stream instead of GetEventItem --
    // `wait_for_cop_finished_and_result_data` captures both without
    // assuming which arrives first (see its own doc comment).
    let result = wait_for_cop_finished_and_result_data(&mut events, 2000).await;

    assert_eq!(server.backdoor.five_baud_init_count(), 1);
    assert_eq!(server.backdoor.fast_init_count(), 0);
    assert_eq!(
        server.backdoor.five_baud_init_input(MOCK_CHANNEL_ID),
        Some(0x42),
        "the address should come from the preset CP_5BaudAddressPhys, not cop_data"
    );

    assert_result_data(&result, &[], &[], &[0x55, 0x8F]);

    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE).await,
        MOCK_FIVE_BAUD_NEGOTIATED_BAUD
    );
    assert_eq!(server.backdoor.start_periodic_count(), 0);
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 1);

    drop(events);
    server.shutdown().await;
}

/// ADR-076: the spec 5-baud contract applies to BOTH K-line protocols --
/// ISO9141 with an explicit `CP_InitializationSettings=1` sources the
/// address from `CP_5BaudAddressPhys` (not `cop_data`, which must be
/// empty), delivers the raw keybytes when `NumReceiveCycles=1`, and writes
/// the negotiated baud rate back so `GetComParam(CP_Baudrate)` reflects it.
/// Complements `iso14230_spec_five_baud_contract_happy_path` (spec path on
/// the other K-line protocol) and
/// `iso9141_legacy_heuristic_uses_raw_cop_data_byte_as_address` (legacy
/// path on this one).
#[tokio::test]
#[serial]
async fn iso9141_spec_five_baud_contract_happy_path() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO9141, 1).await;
    // Distinct from `MOCK_FIVE_BAUD_NEGOTIATED_BAUD` so the write-back below
    // is observable.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 9_600).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    set_com_param_unum32(&mut client, cll_handle, CP_5BAUD_ADDR_PHYS, 0x2A).await;
    promote_via_update_param(&mut client, cll_handle).await;

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
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
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    // ADR-115 single-consumer correction: a live subscriber is attached
    // above, so both PduCopstFinished and this response's ResultData are
    // delivered on the live stream instead of GetEventItem --
    // `wait_for_cop_finished_and_result_data` captures both without
    // assuming which arrives first (see its own doc comment).
    let result = wait_for_cop_finished_and_result_data(&mut events, 2000).await;

    assert_eq!(server.backdoor.five_baud_init_count(), 1);
    assert_eq!(server.backdoor.fast_init_count(), 0);
    assert_eq!(
        server.backdoor.five_baud_init_input(MOCK_CHANNEL_ID),
        Some(0x2A),
        "the address should come from CP_5BaudAddressPhys, not cop_data"
    );

    assert_result_data(&result, &[], &[], &[0x55, 0x8F]);

    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE).await,
        MOCK_FIVE_BAUD_NEGOTIATED_BAUD
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-076: `CP_RequestAddrMode=2` (functional addressing) selects
/// `CP_5BaudAddressFunc` instead of `CP_5BaudAddressPhys` for the spec
/// 5-baud contract's target address.
#[tokio::test]
#[serial]
async fn iso14230_spec_five_baud_uses_functional_address_when_functional_addressing_selected() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_5BAUD_ADDR_FUNC, 0x77).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
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
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish"
    );

    assert_eq!(
        server.backdoor.five_baud_init_input(MOCK_CHANNEL_ID),
        Some(0x77),
        "functional addressing should select CP_5BaudAddressFunc"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-076: an explicit `CP_InitializationSettings=1` is inert on a
/// non-K-line link -- no init sequence ever runs outside ISO9141/ISO14230,
/// so none of the spec 5-baud contract's call-time validation applies either:
/// a `NumReceiveCycles` outside `{0, 1}` is not rejected, no 5-baud address is
/// resolved, and the COP completes without any adapter init call. Uses an
/// empty `cop_data` deliberately -- a non-empty `cop_data` on CAN is now the
/// optional-message transmit path (ADR-111, `startcomm_optional_message_tx.rs`
/// covers that separately), which is orthogonal to the 5-baud-inertness claim
/// this test exists for.
#[tokio::test]
#[serial]
async fn can_explicit_five_baud_setting_is_inert_on_non_k_line_link() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // `num_receive_cycles=5` would be a synchronous INVALID_ARGUMENT under
    // the K-line spec 5-baud contract; on CAN it is not consulted at all
    // (empty cop_data keeps ADR-111's optional-message path inert too, so
    // this is a pure test of the 5-baud-settings-are-inert claim).
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 5,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed on a non-K-line link");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish"
    );

    assert_eq!(server.backdoor.five_baud_init_count(), 0);
    assert_eq!(server.backdoor.fast_init_count(), 0);

    drop(events);
    server.shutdown().await;
}

/// ADR-076: `NumReceiveCycles=0` runs the 5-baud init (the mock's counter
/// still increments and `CP_Baudrate` still reflects the negotiated rate),
/// but suppresses ECU key byte delivery entirely -- no `ResultData` event is
/// emitted, and the COP still finishes normally.
#[tokio::test]
#[serial]
async fn iso14230_spec_five_baud_num_receive_cycles_zero_suppresses_keybyte_delivery() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

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
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
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
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish even though keybyte delivery is suppressed"
    );

    assert_eq!(
        server.backdoor.five_baud_init_count(),
        1,
        "the 5-baud init should still run with NumReceiveCycles=0"
    );
    assert_no_result_data(
        &mut client,
        cll_handle,
        "NumReceiveCycles=0 must suppress ECU key byte delivery",
    )
    .await;

    drop(events);
    server.shutdown().await;
}

/// ADR-076: any `NumReceiveCycles` value other than `0` or `1` is rejected
/// synchronously with `INVALID_ARGUMENT` for the spec 5-baud contract -- the
/// COP never reaches the adapter.
#[tokio::test]
#[serial]
async fn iso14230_spec_five_baud_rejects_invalid_num_receive_cycles() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 10_400).await;
    set_com_param_unum32(&mut client, cll_handle, CP_INIT_SETTINGS, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    for &num_receive_cycles in &[2i32, -1i32] {
        let status = client
            .start_com_primitive(StartComPrimitiveRequest {
                cop_tag: None,
                cll_handle: Some(cll_handle),
                cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
                cop_data: vec![],
                cop_ctrl_data: Some(ComPrimitiveCtrlData {
                    time: 0,
                    num_send_cycles: 0,
                    num_receive_cycles,
                    temp_param_update: 0,
                    expected_response_array: Vec::<ExpectedResponseData>::new(),
                    tx_flag: None,
                }),
            })
            .await
            .expect_err(&format!(
                "num_receive_cycles={num_receive_cycles} should be rejected for a 5-baud init"
            ));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    assert_eq!(
        server.backdoor.five_baud_init_count(),
        0,
        "a rejected COP must never reach the adapter"
    );

    server.shutdown().await;
}

/// ADR-076: `CP_5BaudAddressFunc`/`CP_5BaudAddressPhys` are single address
/// bytes sent at 5 baud, so `SetComParam` rejects a value that does not fit
/// in one byte -- mirroring the `CP_InitializationSettings` range check
/// (ADR-074).
#[tokio::test]
#[serial]
async fn set_com_param_5baud_address_rejects_out_of_range_values() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;

    for &com_param_id in &[CP_5BAUD_ADDR_PHYS, CP_5BAUD_ADDR_FUNC] {
        let status = client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                    com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(0x100)),
                }),
            })
            .await
            .expect_err(&format!(
                "SetComParam({com_param_id:#x}=0x100) should be rejected"
            ));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    // The upper boundary (0xFF) is accepted.
    for &com_param_id in &[CP_5BAUD_ADDR_PHYS, CP_5BAUD_ADDR_FUNC] {
        client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                    com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(0xFF)),
                }),
            })
            .await
            .unwrap_or_else(|_| panic!("SetComParam({com_param_id:#x}=0xFF) should be accepted"));
    }

    server.shutdown().await;
}

/// PR #53 review round (Fix 2), corrected in a later round: `CP_PhysReqFormatPriorityType`/
/// `CP_PhysReqTargetAddr`/`CP_FuncReqFormatPriorityType`/
/// `CP_FuncReqTargetAddr`/`CP_Node_Address` are single address/format bytes
/// `tx_header::kwp_header_bytes` narrows with `as u8` on a KWP-family
/// (ISO9141/ISO14230) link -- `SetComParam` rejects a value that does not
/// fit in one byte, mirroring the `CP_5BaudAddressFunc`/`CP_5BaudAddressPhys`
/// range check (`set_com_param_5baud_address_rejects_out_of_range_values`)
/// above. This test connects via ISO14230, so it exercises the KWP-family
/// side of the check only -- see
/// `set_com_param_j1850_addressing_rejects_out_of_range_values` for the
/// J1850-family side, and `set_com_param_can_addressing_byte_ids_accept_values_above_0xff`
/// for the CAN-link regression these two together are meant to guard
/// against (CAN also allows `SetComParam` of these five IDs but never
/// forwards/truncates them, so it must not reject).
#[tokio::test]
#[serial]
async fn set_com_param_kwp_addressing_rejects_out_of_range_values() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO14230, 1).await;

    for &com_param_id in &[
        CP_PHYS_REQ_TARGET_ADDR,
        CP_FUNC_REQ_FORMAT_PRIORITY,
        j2534_0404::NODE_ADDRESS,
    ] {
        let status = client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                    com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(0x133)),
                }),
            })
            .await
            .expect_err(&format!(
                "SetComParam({com_param_id:#x}=0x133) should be rejected"
            ));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    // The upper boundary (0xFF) and an ordinary in-range value (0x10) are
    // both accepted.
    for &com_param_id in &[
        CP_PHYS_REQ_TARGET_ADDR,
        CP_FUNC_REQ_FORMAT_PRIORITY,
        j2534_0404::NODE_ADDRESS,
    ] {
        for value in [0x10u32, 0xFF] {
            client
                .set_com_param(SetComParamRequest {
                    cll_handle: Some(cll_handle),
                    param_item: Some(ParamItem {
                        id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                        com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                        param_data: Some(param_item::ParamData::Unum32(value)),
                    }),
                })
                .await
                .unwrap_or_else(|_| {
                    panic!("SetComParam({com_param_id:#x}={value:#x}) should be accepted")
                });
        }
    }

    server.shutdown().await;
}

/// PR #53 review round (Fix 2), corrected in a later round: the J1850-family
/// side of the same `CP_PhysReqFormatPriorityType`/`CP_PhysReqTargetAddr`/
/// `CP_FuncReqFormatPriorityType`/`CP_FuncReqTargetAddr`/`CP_Node_Address`
/// range check as `set_com_param_kwp_addressing_rejects_out_of_range_values`
/// -- `tx_header::j1850_header_bytes` narrows each with `as u8` on a
/// J1850PWM/J1850VPW link, so `SetComParam` rejects a value that does not fit
/// in one byte here too.
#[tokio::test]
#[serial]
async fn set_com_param_j1850_addressing_rejects_out_of_range_values() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::J1850VPW, 1).await;

    for &com_param_id in &[
        CP_PHYS_REQ_TARGET_ADDR,
        CP_FUNC_REQ_FORMAT_PRIORITY,
        j2534_0404::NODE_ADDRESS,
    ] {
        let status = client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                    com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(0x133)),
                }),
            })
            .await
            .expect_err(&format!(
                "SetComParam({com_param_id:#x}=0x133) should be rejected"
            ));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    // The upper boundary (0xFF) and an ordinary in-range value (0x10) are
    // both accepted.
    for &com_param_id in &[
        CP_PHYS_REQ_TARGET_ADDR,
        CP_FUNC_REQ_FORMAT_PRIORITY,
        j2534_0404::NODE_ADDRESS,
    ] {
        for value in [0x10u32, 0xFF] {
            client
                .set_com_param(SetComParamRequest {
                    cll_handle: Some(cll_handle),
                    param_item: Some(ParamItem {
                        id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                        com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                        param_data: Some(param_item::ParamData::Unum32(value)),
                    }),
                })
                .await
                .unwrap_or_else(|_| {
                    panic!("SetComParam({com_param_id:#x}={value:#x}) should be accepted")
                });
        }
    }

    server.shutdown().await;
}

/// `_PS`-qualified counterpart of `set_com_param_kwp_addressing_rejects_out_of_range_values`:
/// connects via the raw `PROTOCOL_ISO9141_PS` protocol id with NON-default
/// `dlc_pin_data` (pins 1/"K" and 8/"L", differing from ISO9141's own default
/// 7/"K" and 15/"L") -- an exact structural match against the default pins
/// makes `resolve_pin_selection` canonicalize to `Ok(None)` (its own doc
/// comment, `names.rs`), which sets `LogicalLinkState::hw_protocol_id`
/// straight to the plain base id with `base_hw_protocol_override` left
/// `None` -- i.e. NOT actually a `_PS`-qualified link at the `hw_protocol_id`
/// level, so that pin choice would exercise nothing this test exists to
/// cover. Non-default pins keep Pin Selection genuinely active:
/// `hw_protocol_id` becomes the real `PROTOCOL_ISO9141_PS` id and
/// `base_hw_protocol_override` becomes `Some(ISO9141)`, so this proves the
/// range check holds once `check_param_allowed`'s
/// `ChannelProtocol::from_raw(link.base_hw_protocol_id())` actually
/// unwraps that override back to the KWP-family base id -- previously
/// confirmed only by code tracing, not a dedicated test (Codex review
/// finding, PR #87: an earlier version of this test used the exact default
/// pins, which canonicalized away the very `_PS`-ness it meant to cover).
#[tokio::test]
#[serial]
async fn set_com_param_kwp_addressing_rejects_out_of_range_values_on_ps_qualified_link() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        MOCK_MODULE_HANDLE,
        resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_ISO9141_PS, &[(1, "K"), (8, "L")]),
        1,
    )
    .await
    .expect("create_com_logical_link should succeed for a well-formed Pin Selection request");

    for &com_param_id in &[
        CP_PHYS_REQ_TARGET_ADDR,
        CP_PHYS_REQ_FORMAT_PRIORITY,
        CP_FUNC_REQ_TARGET_ADDR,
        CP_FUNC_REQ_FORMAT_PRIORITY,
        j2534_0404::NODE_ADDRESS,
    ] {
        let status = client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                    com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(0x133)),
                }),
            })
            .await
            .expect_err(&format!(
                "SetComParam({com_param_id:#x}=0x133) should be rejected"
            ));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    // The upper boundary (0xFF) and an ordinary in-range value (0x10) are
    // both accepted.
    for &com_param_id in &[
        CP_PHYS_REQ_TARGET_ADDR,
        CP_PHYS_REQ_FORMAT_PRIORITY,
        CP_FUNC_REQ_TARGET_ADDR,
        CP_FUNC_REQ_FORMAT_PRIORITY,
        j2534_0404::NODE_ADDRESS,
    ] {
        for value in [0x10u32, 0xFF] {
            client
                .set_com_param(SetComParamRequest {
                    cll_handle: Some(cll_handle),
                    param_item: Some(ParamItem {
                        id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                        com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                        param_data: Some(param_item::ParamData::Unum32(value)),
                    }),
                })
                .await
                .unwrap_or_else(|_| {
                    panic!("SetComParam({com_param_id:#x}={value:#x}) should be accepted")
                });
        }
    }

    server.shutdown().await;
}

/// `_PS`-qualified counterpart of `set_com_param_j1850_addressing_rejects_out_of_range_values`:
/// connects via the raw `PROTOCOL_J1850VPW_PS` protocol id with a NON-default
/// primary pin (pin 1/"PLUS", differing from J1850VPW's own default pin
/// 2/"PLUS") -- same reasoning as
/// `set_com_param_kwp_addressing_rejects_out_of_range_values_on_ps_qualified_link`'s
/// own doc comment: an exact default-pins match canonicalizes away the
/// `_PS`-ness this test exists to cover (Codex review finding, PR #87).
/// Proves the same range check holds once `check_param_allowed`'s
/// `ChannelProtocol::from_raw(link.base_hw_protocol_id())` actually unwraps
/// `base_hw_protocol_override` back to the J1850-family base id --
/// previously confirmed only by code tracing, not a dedicated test.
#[tokio::test]
#[serial]
async fn set_com_param_j1850_addressing_rejects_out_of_range_values_on_ps_qualified_link() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        MOCK_MODULE_HANDLE,
        resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_J1850VPW_PS, &[(1, "PLUS")]),
        1,
    )
    .await
    .expect("create_com_logical_link should succeed for a well-formed Pin Selection request");

    for &com_param_id in &[
        CP_PHYS_REQ_TARGET_ADDR,
        CP_PHYS_REQ_FORMAT_PRIORITY,
        CP_FUNC_REQ_TARGET_ADDR,
        CP_FUNC_REQ_FORMAT_PRIORITY,
        j2534_0404::NODE_ADDRESS,
    ] {
        let status = client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                    com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(0x133)),
                }),
            })
            .await
            .expect_err(&format!(
                "SetComParam({com_param_id:#x}=0x133) should be rejected"
            ));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    // The upper boundary (0xFF) and an ordinary in-range value (0x10) are
    // both accepted.
    for &com_param_id in &[
        CP_PHYS_REQ_TARGET_ADDR,
        CP_PHYS_REQ_FORMAT_PRIORITY,
        CP_FUNC_REQ_TARGET_ADDR,
        CP_FUNC_REQ_FORMAT_PRIORITY,
        j2534_0404::NODE_ADDRESS,
    ] {
        for value in [0x10u32, 0xFF] {
            client
                .set_com_param(SetComParamRequest {
                    cll_handle: Some(cll_handle),
                    param_item: Some(ParamItem {
                        id: Some(vci_service_interface::param_item::Id::ParamId(com_param_id)),
                        com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                        param_data: Some(param_item::ParamData::Unum32(value)),
                    }),
                })
                .await
                .unwrap_or_else(|_| {
                    panic!("SetComParam({com_param_id:#x}={value:#x}) should be accepted")
                });
        }
    }

    server.shutdown().await;
}

/// Regression test for the CAN-link scoping bug found in an edge-case-hunter
/// pass on PR #53: `CP_PhysReqTargetAddr` (and the other four addressing
/// IDs above) is also `SetComParam`-allowed on CAN
/// (`comparam_support::is_can_param`), but nothing on the CAN forwarding
/// path ever reads or truncates it (only `tx_header::j1850_header_bytes`/
/// `kwp_header_bytes` do, and neither is on the CAN code path), so a value
/// above 0xFF must be accepted on a CAN link, not rejected by the
/// J1850/KWP-only `value > 0xFF` check above.
#[tokio::test]
#[serial]
async fn set_com_param_can_addressing_byte_ids_accept_values_above_0xff() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;

    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    CP_PHYS_REQ_TARGET_ADDR,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(0x133)),
            }),
        })
        .await
        .expect("SetComParam(CP_PhysReqTargetAddr=0x133) should be accepted on a CAN link");

    server.shutdown().await;
}

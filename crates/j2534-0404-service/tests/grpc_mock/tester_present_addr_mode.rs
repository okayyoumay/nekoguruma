//! ADR-138: `CP_TesterPresentAddrMode` selects tester-present's own
//! functional-vs-physical addressing, independently of `CP_RequestAddrMode`
//! (the general `CoptSendrecv`/init-flags addressing ComParam, ADR-054).
//! Before this ADR, tester-present silently inherited `CP_RequestAddrMode`'s
//! resolved addressing; these tests confirm the two ComParams are now
//! genuinely independent knobs.

use serial_test::serial;
use vci_service_interface::{
    ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, ModuleHandle,
    StartComPrimitiveRequest, create_com_logical_link_request, event_item, subscribe_event_request,
};

use crate::harness::*;

// KWP addressing ComParams (`tx_header.rs::kwp_header_bytes`) used by the
// KWP tests below: `CP_PhysReqFormatPriorityType`/`CP_PhysReqTargetAddr`/
// `CP_FuncReqFormatPriorityType`/`CP_FuncReqTargetAddr` now live in
// `harness.rs` as shared consts (`CP_PHYS_REQ_FORMAT_PRIORITY` etc.,
// brought into scope by the glob import above) -- promoted out of this
// module's own former per-file duplicate once `comparam_tx.rs` needed the
// same IDs too (backlog fix: `comparam_support::is_j1850vpw_param`/
// `is_j1850pwm_param` now allow `SetComParam` on these IDs for J1850 as
// well, so they are no longer KWP-only). The J1850 tests below still use
// `create_and_connect_cll_by_resource_name`'s resource-preset addressing
// (its own doc comment explains why that convention is kept here even
// though `SetComParam` is a viable alternative now).

/// Waits for the next `PduCopstFinished` on an already-open event stream.
/// The stream must be subscribed BEFORE the COP being waited on is started,
/// so its `PduCopstFinished` cannot be missed. Duplicated locally per this
/// suite's own convention of not coupling test modules to each other's
/// helpers (see `tester_present_send_type.rs`'s identical helper).
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

async fn start_comm(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) {
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
}

/// Creates and connects a CLL by resource name (`GetResourceIds`/
/// `CreateComLogicalLink`'s static resource table, `service::resources`)
/// instead of a bare protocol id. Used by the J1850 tests below to get
/// distinct functional-vs-physical addressing bytes from a resource
/// preset's own seeded defaults
/// (`comparam_defaults::iso_15031_5_on_sae_j1850_vpw`) without an extra
/// `SetComParam`/`CoptUpdateparam` round trip per test. `SetComParam` on
/// `CP_FuncReqFormatPriorityType`/`CP_FuncReqTargetAddr`/
/// `CP_PhysReqFormatPriorityType`/`CP_PhysReqTargetAddr` is now also allowed
/// for J1850 (`comparam_support::is_j1850vpw_param`/`is_j1850pwm_param`,
/// backlog fix -- see `comparam_tx.rs`'s
/// `j1850vpw_set_com_param_overrides_physical_addressing_on_wire` for a
/// bare-protocol CLL exercising that path directly), but this helper's
/// preset-based approach is kept here since these tests want two already-
/// distinct addressing values without extra setup calls.
async fn create_and_connect_cll_by_resource_name(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource_name: &str,
) -> vci_service_interface::ComLogicalLinkHandle {
    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::ResourceName(
                resource_name.to_string(),
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

/// `CP_TesterPresentAddrMode = 1` (functional) with `CP_RequestAddrMode`
/// left at its physical default: `CoptStartcomm`'s arm-time tester-present
/// send uses the functional CAN ID (`CP_CanFuncReqId`), while a
/// `CoptSendrecv` on the same link still uses the UniqueRespIdTable's
/// physical CAN ID -- the two ComParams resolve independently.
#[tokio::test]
#[serial]
async fn tester_present_addr_mode_1_sends_functionally_while_sendrecv_stays_physical() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id(&mut client, cll_handle, 0x7E0).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    // Large enough that no second, due-triggered tester-present send lands
    // inside this test's own window.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 10_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut expected_tp = 0x7DF_u32.to_be_bytes().to_vec();
    expected_tp.push(0x3E);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_tp,
        "CoptStartcomm's arm-time tester-present send should use the functional CAN ID selected \
         by CP_TesterPresentAddrMode = 1, independently of CP_RequestAddrMode"
    );

    send_data(&mut client, cll_handle, vec![0x02, 0x10, 0x03], vec![]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let mut expected_sendrecv = 0x7E0_u32.to_be_bytes().to_vec();
    expected_sendrecv.extend_from_slice(&[0x02, 0x10, 0x03]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_sendrecv,
        "CoptSendrecv should still use CP_RequestAddrMode's own (physical, default) resolution, \
         unaffected by CP_TesterPresentAddrMode"
    );

    drop(events);
    server.shutdown().await;
}

/// Converse of the above: `CP_RequestAddrMode = 2` (functional) with
/// `CP_TesterPresentAddrMode` left at its physical default: tester-present
/// sends physically (via the UniqueRespIdTable) while `CoptSendrecv` sends
/// functionally.
#[tokio::test]
#[serial]
async fn request_addr_mode_2_does_not_make_tester_present_functional() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id(&mut client, cll_handle, 0x7E0).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    // CP_TesterPresentAddrMode intentionally left unset (physical default).
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 10_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut expected_tp = 0x7E0_u32.to_be_bytes().to_vec();
    expected_tp.push(0x3E);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_tp,
        "CoptStartcomm's tester-present send should stay physical (CP_TesterPresentAddrMode's own \
         default) even though CP_RequestAddrMode is functional"
    );

    send_data(&mut client, cll_handle, vec![0x02, 0x10, 0x03], vec![]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    let mut expected_sendrecv = 0x7DF_u32.to_be_bytes().to_vec();
    expected_sendrecv.extend_from_slice(&[0x02, 0x10, 0x03]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_sendrecv,
        "CoptSendrecv should send functionally per CP_RequestAddrMode = 2"
    );

    drop(events);
    server.shutdown().await;
}

/// A live `CoptUpdateparam` that changes only `CP_TesterPresentAddrMode` on
/// an already-`Armed`, comm-started CLL re-arms and sends immediately with
/// the newly-resolved addressing. `CP_TesterPresentAddrMode` is not itself a
/// member of `same_wire_behavior`'s comparison set (ADR-138): this proves
/// the load-bearing claim that the flip is caught anyway, because it changes
/// `data` (a different CAN ID) and `can_functional` in the freshly-resolved
/// struct, both of which `same_wire_behavior` already compares.
#[tokio::test]
#[serial]
async fn live_tester_present_addr_mode_change_rearms_with_new_addressing() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id(&mut client, cll_handle, 0x7E0).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 10_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    // CP_TesterPresentAddrMode starts unset (physical default).
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let mut expected_physical = 0x7E0_u32.to_be_bytes().to_vec();
    expected_physical.push(0x3E);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_physical,
        "the initial arm-time send should be physical (CP_TesterPresentAddrMode's default)"
    );

    // Live change: CP_TesterPresentAddrMode 0 (absent) -> 1 (functional),
    // nothing else touched.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "a live CP_TesterPresentAddrMode flip should re-arm and send immediately, the same \
         \"just became enabled\" contract other tester-present-relevant CoptUpdateparam changes \
         give (ADR-084/ADR-138)"
    );
    let mut expected_functional = 0x7DF_u32.to_be_bytes().to_vec();
    expected_functional.push(0x3E);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        expected_functional,
        "the re-armed send should carry the newly-resolved functional addressing"
    );

    drop(events);
    server.shutdown().await;
}

/// `CP_TesterPresentAddrMode = 1` with no `CP_CanFuncReqId` configured, and
/// `CP_RequestAddrMode` left physical: `CoptStartcomm` synchronously rejects
/// with `INVALID_ARGUMENT` naming `CP_TesterPresentAddrMode` (so a client
/// whose `CP_RequestAddrMode` is physical isn't left wondering why it's
/// being told the send is functional), while a plain `CoptSendrecv` on the
/// same link -- which doesn't need functional addressing -- still succeeds.
#[tokio::test]
#[serial]
async fn tester_present_addr_mode_1_without_can_func_req_id_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_can_phys_req_id(&mut client, cll_handle, 0x7E0).await;
    // CP_CanFuncReqId is deliberately left unset.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "CoptStartcomm should reject CP_TesterPresentAddrMode = 1 with no CP_CanFuncReqId",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("CP_TesterPresentAddrMode"),
        "{}",
        status.message()
    );
    assert!(
        status.message().contains("CP_CanFuncReqId"),
        "{}",
        status.message()
    );
    assert_eq!(server.backdoor.written_count(MOCK_CHANNEL_ID), 0);

    // A plain CoptSendrecv (physical, already fully configured via the
    // UniqueRespIdTable) is unaffected by tester-present's own resolution
    // failure.
    send_data(&mut client, cll_handle, vec![0x02, 0x10, 0x03], vec![]).await;
    let mut expected_sendrecv = 0x7E0_u32.to_be_bytes().to_vec();
    expected_sendrecv.extend_from_slice(&[0x02, 0x10, 0x03]);
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        expected_sendrecv,
        "CoptSendrecv should still succeed physically even though CoptStartcomm was rejected"
    );

    server.shutdown().await;
}

// ── KWP/J1850 coverage ──────────────────────────────────────────────────────
//
// The CAN/ISO15765 tests above only exercise `CanAddressing`
// (`tx_header::resolve_can_addressing`), whose `functional` flag also
// populates `ResolvedTesterPresent::can_functional` -- a field
// `same_wire_behavior` (`rpc_primitive.rs`) compares directly. KWP/J1850
// never populate `can_functional` (it stays `None` for both physical and
// functional addressing on these protocol families), so the tests below
// prove the same independence and live-re-arm contract hold there too,
// driven entirely by `same_wire_behavior`'s `data` comparison (the full
// built tx message, header included) rather than by `can_functional`.

/// KWP (ISO14230) counterpart of
/// `tester_present_addr_mode_1_sends_functionally_while_sendrecv_stays_physical`:
/// `CP_TesterPresentAddrMode = 1` selects `CP_FuncReqFormatPriorityType`/
/// `CP_FuncReqTargetAddr` for the tester-present header
/// (`tx_header::kwp_header_bytes`), independently of `CP_RequestAddrMode`
/// staying physical (which still resolves `CP_PhysReqFormatPriorityType`/
/// `CP_PhysReqTargetAddr` for `CoptSendrecv`). `CP_InitializationSettings =
/// 3` skips ISO14230's K-line init sequence (matching this suite's sibling
/// files' own convention, e.g. `tester_present_send_type.rs`'s
/// `kline_five_baud_init_stamps_last_bus_activity_deferring_mode_1_sibling`),
/// so the only traffic on the mock channel is the arm-time tester-present
/// send and the explicit `CoptSendrecv` below.
#[tokio::test]
#[serial]
async fn kwp_tester_present_addr_mode_1_sends_functionally_while_sendrecv_stays_physical() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400), (CP_INIT_SETTINGS, 3)],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_PHYS_REQ_FORMAT_PRIORITY, 0x81).await;
    set_com_param_unum32(&mut client, cll_handle, CP_PHYS_REQ_TARGET_ADDR, 0x11).await;
    set_com_param_unum32(&mut client, cll_handle, CP_FUNC_REQ_FORMAT_PRIORITY, 0xC1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_FUNC_REQ_TARGET_ADDR, 0x34).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    // Large enough that no second, due-triggered tester-present send lands
    // inside this test's own window.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 10_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // KWP header: format (0xC1, ADR-166 embedded-length mode -- low 6 bits
    // configured nonzero, so the payload length recomposes into the format
    // byte itself instead of a separate trailing length byte), target,
    // source -- 0x3E is the 1-byte tester-present payload
    // (`tx_header::kwp_header_bytes`). 0xC1's low 6 bits already equal the
    // actual payload length (1), so the recomposed format byte is
    // byte-identical to the configured one here.
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        vec![0xC1, 0x34, 0xF1, 0x3E],
        "CoptStartcomm's arm-time tester-present send should use the functional KWP header \
         (CP_FuncReqFormatPriorityType/CP_FuncReqTargetAddr) selected by \
         CP_TesterPresentAddrMode = 1, independently of CP_RequestAddrMode"
    );

    send_data(&mut client, cll_handle, vec![0x02, 0x10, 0x03], vec![]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        // CP_PhysReqFormatPriorityType is 0x81 (ADR-166 embedded-length mode
        // too) -- the 3-byte payload recomposes the format byte's low 6 bits
        // to 0x03, giving 0x83, with no separate trailing length byte.
        vec![0x83, 0x11, 0xF1, 0x02, 0x10, 0x03],
        "CoptSendrecv should still use CP_RequestAddrMode's own (physical, default) resolution \
         (CP_PhysReqFormatPriorityType/CP_PhysReqTargetAddr), unaffected by \
         CP_TesterPresentAddrMode"
    );

    drop(events);
    server.shutdown().await;
}

/// KWP (ISO14230) counterpart of
/// `live_tester_present_addr_mode_change_rearms_with_new_addressing`: a live
/// `CoptUpdateparam` that changes only `CP_TesterPresentAddrMode` on an
/// already-`Armed`, comm-started KWP CLL re-arms and sends immediately with
/// the newly-resolved KWP header. Unlike the CAN/ISO15765 case,
/// `ResolvedTesterPresent::can_functional` is structurally `None` for KWP
/// both before and after the flip (only CAN/ISO15765 populate that field) --
/// this proves the re-arm is driven entirely by `same_wire_behavior`'s
/// comparison of `data` (the full built tx message, header included), not by
/// `can_functional`.
#[tokio::test]
#[serial]
async fn kwp_live_tester_present_addr_mode_change_rearms_with_new_addressing() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400), (CP_INIT_SETTINGS, 3)],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_PHYS_REQ_FORMAT_PRIORITY, 0x81).await;
    set_com_param_unum32(&mut client, cll_handle, CP_PHYS_REQ_TARGET_ADDR, 0x11).await;
    set_com_param_unum32(&mut client, cll_handle, CP_FUNC_REQ_FORMAT_PRIORITY, 0xC1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_FUNC_REQ_TARGET_ADDR, 0x34).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 10_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    // CP_TesterPresentAddrMode starts unset (physical default).
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        // 0x81's low 6 bits (ADR-166 embedded-length mode) already equal
        // the actual 1-byte payload length, so the recomposed format byte
        // is byte-identical to the configured one, with no separate
        // trailing length byte.
        vec![0x81, 0x11, 0xF1, 0x3E],
        "the initial arm-time send should be physical (CP_TesterPresentAddrMode's default)"
    );

    // Live change: CP_TesterPresentAddrMode 0 (absent) -> 1 (functional),
    // nothing else touched.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "a live CP_TesterPresentAddrMode flip should re-arm and send immediately on KWP too, \
         even though can_functional stays None throughout -- same_wire_behavior's data \
         comparison alone must catch the header change (ADR-084/ADR-138)"
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        // 0xC1's low 6 bits (ADR-166 embedded-length mode) already equal
        // the actual 1-byte payload length, so the recomposed format byte
        // is byte-identical to the configured one.
        vec![0xC1, 0x34, 0xF1, 0x3E],
        "the re-armed send should carry the newly-resolved functional KWP header"
    );

    drop(events);
    server.shutdown().await;
}

/// ADR-138 correction (Codex review, PR #158): a KWP CLL whose
/// `CP_PhysReqFormatPriorityType`/`CP_PhysReqTargetAddr` are explicitly
/// configured identical to `CP_FuncReqFormatPriorityType`/
/// `CP_FuncReqTargetAddr` produces byte-identical KWP headers for a
/// physical vs. functional `CP_TesterPresentAddrMode` resolution --
/// `can_functional` stays `None` for KWP either way, so before this fix
/// `same_wire_behavior` saw no difference at all (neither `data` nor
/// `can_functional` changed) and skipped the resend NOTE 1 requires. This
/// proves the live re-arm still happens, driven by the new
/// `addr_mode_functional` field, even though the two sends are byte-for-byte
/// identical on the wire.
#[tokio::test]
#[serial]
async fn kwp_byte_coincident_addr_mode_change_still_rearms() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO14230,
        &[(j2534_0404::DATA_RATE, 10_400), (CP_INIT_SETTINGS, 3)],
    )
    .await;
    // Deliberately identical phys/func format+target: the byte-coincidence
    // premise this test depends on.
    set_com_param_unum32(&mut client, cll_handle, CP_PHYS_REQ_FORMAT_PRIORITY, 0x80).await;
    set_com_param_unum32(&mut client, cll_handle, CP_PHYS_REQ_TARGET_ADDR, 0x10).await;
    set_com_param_unum32(&mut client, cll_handle, CP_FUNC_REQ_FORMAT_PRIORITY, 0x80).await;
    set_com_param_unum32(&mut client, cll_handle, CP_FUNC_REQ_TARGET_ADDR, 0x10).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 10_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    // CP_TesterPresentAddrMode starts unset (physical default).
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    let initial_send = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        initial_send,
        vec![0x80, 0x10, 0xF1, 0x01, 0x3E],
        "the initial arm-time send should be physical (CP_TesterPresentAddrMode's default)"
    );

    // Live change: CP_TesterPresentAddrMode 0 (absent) -> 1 (functional),
    // nothing else touched. Because CP_PhysReq*/CP_FuncReq* are identical,
    // the resulting header bytes are unchanged from the first send.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "a live CP_TesterPresentAddrMode flip should re-arm and resend even though the resulting \
         header bytes coincide with the first send -- NOTE 1 ties the resend trigger to the \
         ComParam value changing, not to whether the resulting bytes happen to coincide \
         (ADR-138 correction)"
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        initial_send,
        "the re-armed send's header bytes coincide with the first send's -- confirms this test \
         is exercising the byte-coincidence gap, not merely a header content change"
    );

    drop(events);
    server.shutdown().await;
}

/// J1850 (VPW) counterpart of
/// `tester_present_addr_mode_1_sends_functionally_while_sendrecv_stays_physical`:
/// `CP_TesterPresentAddrMode = 1` selects `CP_FuncReqFormatPriorityType`/
/// `CP_FuncReqTargetAddr` for the tester-present header
/// (`tx_header::j1850_header_bytes`), independently of `CP_RequestAddrMode`
/// staying physical (which still resolves `CP_PhysReqFormatPriorityType`/
/// `CP_PhysReqTargetAddr` for `CoptSendrecv`). Connects via the
/// `"ISO_15031_5_on_SAE_J1850_VPW"` resource preset
/// (`create_and_connect_cll_by_resource_name`) to get distinct seeded
/// functional (`0x68`/`0x6A`) and physical (`0x6C`/`0x10`) addressing bytes
/// -- see that helper's doc comment for why a bare-protocol J1850 connect
/// cannot supply these via `SetComParam`. That preset's own
/// `CP_RequestAddrMode` default is functional (`2`, matching a real OBD
/// scan tool's request-broadcast bias), so `CP_RequestAddrMode` is
/// explicitly set to `1` (physical) here to get the "CP_RequestAddrMode
/// stays physical" half of this test's claim -- unlike the CAN/KWP
/// counterparts, where physical is already each protocol's own connect
/// default. J1850 has no K-line init sequence, so `CoptStartcomm` needs no
/// `CP_InitializationSettings` override (unlike the KWP tests above).
#[tokio::test]
#[serial]
async fn j1850_tester_present_addr_mode_1_sends_functionally_while_sendrecv_stays_physical() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle =
        create_and_connect_cll_by_resource_name(&mut client, "ISO_15031_5_on_SAE_J1850_VPW").await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    // Large enough that no second, due-triggered tester-present send lands
    // inside this test's own window.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 10_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // J1850 header: format, target, source -- no length byte
    // (`tx_header::j1850_header_bytes`). 0x68/0x6A are the preset's seeded
    // CP_FuncReqFormatPriorityType/CP_FuncReqTargetAddr.
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        vec![0x68, 0x6A, 0xF1, 0x3E],
        "CoptStartcomm's arm-time tester-present send should use the functional J1850 header \
         (CP_FuncReqFormatPriorityType/CP_FuncReqTargetAddr) selected by \
         CP_TesterPresentAddrMode = 1, independently of CP_RequestAddrMode"
    );

    send_data(&mut client, cll_handle, vec![0x02, 0x10, 0x03], vec![]).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 2).await;
    // 0x6C/0x10 are the preset's seeded CP_PhysReqFormatPriorityType/
    // CP_PhysReqTargetAddr.
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        vec![0x6C, 0x10, 0xF1, 0x02, 0x10, 0x03],
        "CoptSendrecv should still use CP_RequestAddrMode's own (physical) resolution \
         (CP_PhysReqFormatPriorityType/CP_PhysReqTargetAddr), unaffected by \
         CP_TesterPresentAddrMode"
    );

    drop(events);
    server.shutdown().await;
}

/// J1850 (VPW) counterpart of
/// `live_tester_present_addr_mode_change_rearms_with_new_addressing`: a live
/// `CoptUpdateparam` that changes only `CP_TesterPresentAddrMode` on an
/// already-`Armed`, comm-started J1850 CLL re-arms and sends immediately
/// with the newly-resolved J1850 header. Like KWP,
/// `ResolvedTesterPresent::can_functional` is structurally `None` for J1850
/// both before and after the flip (only CAN/ISO15765 populate that field) --
/// this proves the re-arm is driven entirely by `same_wire_behavior`'s
/// comparison of `data` (the full built tx message, header included), not by
/// `can_functional`. Connects via the same `"ISO_15031_5_on_SAE_J1850_VPW"`
/// resource preset as the test above (see
/// `create_and_connect_cll_by_resource_name`'s doc comment), which supplies
/// this test's distinct seeded functional/physical addressing bytes; unlike
/// that test, `CP_RequestAddrMode` is left untouched here since this test
/// never issues a `CoptSendrecv`.
#[tokio::test]
#[serial]
async fn j1850_live_tester_present_addr_mode_change_rearms_with_new_addressing() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle =
        create_and_connect_cll_by_resource_name(&mut client, "ISO_15031_5_on_SAE_J1850_VPW").await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        vec![0x3E],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 10_000_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    // CP_TesterPresentAddrMode starts unset (physical default).
    promote_via_update_param(&mut client, cll_handle).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle).await;
    wait_for_cop_finished(&mut events).await;
    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;

    // 0x6C/0x10 are the preset's seeded CP_PhysReqFormatPriorityType/
    // CP_PhysReqTargetAddr.
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        vec![0x6C, 0x10, 0xF1, 0x3E],
        "the initial arm-time send should be physical (CP_TesterPresentAddrMode's default)"
    );

    // Live change: CP_TesterPresentAddrMode 0 (absent) -> 1 (functional),
    // nothing else touched.
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_ADDR_MODE, 1).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "a live CP_TesterPresentAddrMode flip should re-arm and send immediately on J1850 too, \
         even though can_functional stays None throughout -- same_wire_behavior's data \
         comparison alone must catch the header change (ADR-084/ADR-138)"
    );
    // 0x68/0x6A are the preset's seeded CP_FuncReqFormatPriorityType/
    // CP_FuncReqTargetAddr.
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        vec![0x68, 0x6A, 0xF1, 0x3E],
        "the re-armed send should carry the newly-resolved functional J1850 header"
    );

    drop(events);
    server.shutdown().await;
}

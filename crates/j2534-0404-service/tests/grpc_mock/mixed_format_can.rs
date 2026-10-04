//! `can_channel_mode = "native-mixed"` (SAE J2534-2 clause 8 "Mixed Format
//! Frames on a CAN Network", ADR-160/Phase 3c): an ISO15765-family CLL's
//! primary channel gets `SET_CONFIG(CAN_MIXED_FORMAT, ON)` once at connect
//! instead of relying on the ADR-041 `FLOW_CONTROL_FILTER` UUDT workaround,
//! a UUDT response id gets a genuine `PASS_FILTER` instead of the ADR-041
//! filter, and the device's own native `ProtocolID` tagging on each received
//! frame decides USDT-vs-UUDT routing per frame -- all on ONE physical
//! channel, unlike `"dual-channel"` mode's separate companion channel.
//!
//! ADR-162 Decision 2: a `CP_CanRespUUDTId` match key that collides with a
//! flow-control-eligible `CP_CanRespUSDTId` match key on the same physical
//! channel is rejected under native-mixed mode -- at connect time (both the
//! fail-fast self-check and the authoritative cross-CLL check inside
//! `finalize_connected_link`) and at promote time
//! (`promote_unique_resp_id_table`).
//!
//! ADR-217 adds a second sub-mode, `can_channel_mode =
//! "native-mixed-all-frames"` (`CAN_MIXED_FORMAT_ALL_FRAMES`, value `2`,
//! instead of `_ON`'s `1`): under `ALL_FRAMES`, `FLOW_CONTROL_FILTER` and
//! `PASS_FILTER`/`BLOCK_FILTER` evaluation happens in parallel per frame
//! rather than either/or, so the ADR-162 collision case above is no longer a
//! hazard and the collision check does not apply to this sub-mode -- see the
//! `native_mixed_all_frames_*` tests at the bottom of this file.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComOperationType, ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest,
    DisconnectComLogicalLinkRequest, IoCtlRequest, ModuleHandle, PduError, PinData, ResourceData,
    StartComPrimitiveRequest, SubscribeEventRequest, create_com_logical_link_request,
    error_detail_from_status, event_item, io_ctl_request, resource_data, subscribe_event_request,
};

use crate::harness::*;

/// `PduErrEvtProtErr` predicate for [`wait_for_event`] (ADR-162 Decision 2),
/// mirroring `locks_and_param_classes.rs`'s identical-shape
/// `is_rsc_locked_error` helper (this suite's per-file-duplication
/// convention for this shape).
fn is_prot_err_error(item: &vci_service_interface::EventItem) -> bool {
    matches!(
        item.data,
        Some(event_item::Data::ErrorData(error))
            if error == vci_service_interface::PduErrorEvent::PduErrEvtProtErr as i32
    )
}

/// Waits for the next `PduCopstFinished` on an already-open event stream
/// (must be subscribed BEFORE the COP being waited on is started) -- mirrors
/// the identical helper duplicated across this test suite's other files
/// (e.g. `locks_and_param_classes.rs`).
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

/// Same shape as `clear_msg_filters.rs`'s own local helper (not shared via
/// `harness.rs`, matching this codebase's existing per-file convention for
/// this specific shape).
async fn clear_msg_filters(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
) {
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                j2534_0404::CLEAR_MSG_FILTERS,
            )),
            input_data: None,
            has_output: false,
        })
        .await
        .expect("io_ctl(CLEAR_MSG_FILTERS) should succeed");
}

/// Decision 1: an ISO15765-family CLL connecting under `native-mixed` issues
/// `SET_CONFIG(CAN_MIXED_FORMAT, ON)` once, at physical-channel creation.
#[tokio::test]
#[serial]
async fn native_mixed_mode_enables_can_mixed_format_at_connect() {
    let server = TestServer::start_with_can_mode(Some("native-mixed")).await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_CAN_MIXED_FORMAT),
        1,
        "native-mixed mode should SET_CONFIG(CAN_MIXED_FORMAT, ON) at connect"
    );
    assert!(
        server
            .backdoor
            .set_config_param_log(MOCK_CHANNEL_ID)
            .contains(&j2534_0404::CONFIG_CAN_MIXED_FORMAT),
        "CONFIG_CAN_MIXED_FORMAT should appear in the connect-time SET_CONFIG log"
    );

    server.shutdown().await;
}

/// Decision 1: `ERR_NOT_SUPPORTED` from the connect-time `SET_CONFIG(
/// CAN_MIXED_FORMAT)` call fails the connect outright, disconnecting the
/// just-opened channel first -- no silent fallback to the ADR-041 workaround.
#[tokio::test]
#[serial]
async fn native_mixed_mode_connect_fails_cleanly_when_device_lacks_support() {
    let server = TestServer::start_with_can_mode(Some("native-mixed")).await;
    let mut client = server.client().await;
    server.backdoor.set_can_mixed_format_unsupported(true);

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err("connect should fail when the device doesn't support CAN_MIXED_FORMAT");

    assert_eq!(status.code(), Code::Internal);
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "PassThruConnect itself should still have been attempted"
    );
    assert_eq!(
        server.backdoor.disconnect_count(),
        1,
        "the just-opened channel should be rolled back on SET_CONFIG failure"
    );

    server.shutdown().await;
}

/// Decision 4: a CLL under `native-mixed` with a `CP_CanRespUUDTId` staged
/// gets a `PASS_FILTER` installed for that id (not a `FLOW_CONTROL_FILTER`);
/// its USDT `CP_CanRespUSDTId` entry keeps the conformant `FLOW_CONTROL_FILTER`
/// unchanged.
#[tokio::test]
#[serial]
async fn native_mixed_mode_installs_pass_filter_for_uudt_id() {
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
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER,
        "USDT keeps the conformant FLOW_CONTROL_FILTER"
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        0x7E8_u32.to_be_bytes()
    );
    assert_eq!(
        server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 0),
        Some(0x7E0_u32.to_be_bytes().to_vec())
    );

    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        j2534_0404::PASS_FILTER,
        "UUDT gets a PASS_FILTER instead of the ADR-041 FLOW_CONTROL_FILTER workaround"
    );
    assert_eq!(
        server.backdoor.filter_mask(MOCK_CHANNEL_ID, 1),
        vec![0xFF, 0xFF, 0xFF, 0xFF]
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 1),
        0x5E8_u32.to_be_bytes()
    );
    assert_eq!(
        server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 1),
        None,
        "a PASS_FILTER has no pFlowControlMsg"
    );

    server.shutdown().await;
}

/// Regression test (edge-case-hunter, ADR-160/Phase 3c post-implementation
/// pass): the native-mixed PASS_FILTER's mask/pattern messages must carry
/// SAE J2534-2 clause 8.2.2.4's paired raw-CAN `ProtocolID` (`CAN`), not the
/// channel's own ISO15765-family connect-time `hw_protocol_id` -- the
/// opposite rule from every other filter type
/// (`install_point_to_point_fc_filter`'s own doc comment). A conformant
/// device rejects the mismatch with `ERR_MSG_PROTOCOL_ID`, silently
/// defeating native-mixed mode entirely; the mock never validated
/// `ProtocolID` against `filter_type`, so this was invisible to
/// `native_mixed_mode_installs_pass_filter_for_uudt_id` above.
#[tokio::test]
#[serial]
async fn native_mixed_mode_pass_filter_uses_can_protocol_id_not_iso15765() {
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
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server
            .backdoor
            .filter_pattern_protocol_id(MOCK_CHANNEL_ID, 1),
        j2534_0404::CAN,
        "SAE J2534-2 clause 8.2.2.4: a PASS_FILTER on a CAN_MIXED_FORMAT-enabled channel must \
         carry the paired raw-CAN ProtocolID, not the channel's own ISO15765 connect-time id"
    );

    server.shutdown().await;
}

/// Decision 5: a CAN-tagged frame matching the configured UUDT id is
/// delivered to the CLL (routed like a dual-channel companion channel's own
/// UUDT-only semantics); a CAN-tagged frame not matching any UUDT id is not.
/// Both arrive on the SAME physical channel as the ISO15765 traffic -- unlike
/// dual-channel mode, native-mixed uses no companion channel at all.
#[tokio::test]
#[serial]
async fn native_mixed_mode_routes_can_tagged_frames_via_uudt_only() {
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
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    // ADR-100 Decision §5 (S8): a receive-only monitor keeps an unbound
    // frame delivered under the unbound-discard model (see `can_mode.rs`'s
    // own dual-channel test for the identical rationale).
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let uudt_payload = vec![0x62, 0xF1, 0x90, 0xAA];
    let uudt_frame = can_frame(0x5E8, &uudt_payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &uudt_frame, j2534_0404::CAN);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x5E8_u32.to_be_bytes(), &[], &uudt_payload);
    assert_eq!(result.unique_resp_identifier, 3);

    // A CAN-tagged frame not matching any configured UUDT id is dropped.
    let unrelated = can_frame(0x123, &[0xAA, 0xBB]);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &unrelated, j2534_0404::CAN);
    assert_no_result_data(
        &mut client,
        cll_handle,
        "a CAN-tagged frame not matching any UUDT id must be dropped under native-mixed",
    )
    .await;

    server.shutdown().await;
}

/// Decision 5: an ISO15765-tagged frame still routes through the existing
/// USDT path unchanged -- proving the new per-frame branch doesn't regress
/// ordinary ISO15765 traffic under native-mixed.
#[tokio::test]
#[serial]
async fn native_mixed_mode_iso15765_tagged_frames_still_route_via_usdt_path() {
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
    send_data(&mut client, cll_handle, vec![0x22, 0xF1, 0x90], vec![]).await;

    let resp_payload = vec![0x62, 0xF1, 0x90, 0xAA];
    let resp = can_frame(0x7E8, &resp_payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &resp, j2534_0404::ISO15765);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x7E8_u32.to_be_bytes(), &[], &resp_payload);
    assert_eq!(result.unique_resp_identifier, 7);

    server.shutdown().await;
}

/// The three pre-existing modes never issue `SET_CONFIG(CAN_MIXED_FORMAT)`
/// at all -- the new connect-time step is entirely gated on
/// `CanChannelMode::NativeMixed`.
#[tokio::test]
#[serial]
async fn other_modes_never_set_can_mixed_format() {
    for mode in ["single-channel", "dual-channel", "software-isotp"] {
        let server = TestServer::start_with_can_mode(Some(mode)).await;
        let mut client = server.client().await;

        let _cll_handle = create_and_connect_cll(
            &mut client,
            j2534_0404::ISO15765,
            &[(j2534_0404::DATA_RATE, 500_000)],
        )
        .await;

        assert!(
            !server
                .backdoor
                .set_config_param_log(MOCK_CHANNEL_ID)
                .contains(&j2534_0404::CONFIG_CAN_MIXED_FORMAT),
            "{mode} must never SET_CONFIG(CAN_MIXED_FORMAT)"
        );

        server.shutdown().await;
    }
}

/// Decision 4: `PDU_IOCTL_CLEAR_MSG_FILTERS` reinstalls the UUDT
/// `PASS_FILTER` correctly -- the reinstall path
/// (`reinstall_iso15765_channel_filters_after_clear`) already targets the
/// same primary channel these new filters live on, so it must not regress.
#[tokio::test]
#[serial]
async fn native_mixed_mode_clear_msg_filters_reinstalls_uudt_pass_filter() {
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
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    clear_msg_filters(&mut client, cll_handle).await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        0x7E8_u32.to_be_bytes()
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        j2534_0404::PASS_FILTER,
        "the UUDT PASS_FILTER must be rebuilt by the CLEAR_MSG_FILTERS reinstall path too"
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 1),
        0x5E8_u32.to_be_bytes()
    );
    assert_eq!(
        server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 1),
        None
    );

    server.shutdown().await;
}

/// ADR-160's explicit FD exclusion: an `FD_ISO15765_PS`-substituted link
/// (ADR-159) never gets native-mixed enabled, even under `native-mixed`
/// mode -- it keeps ADR-159's existing FLOW_CONTROL_FILTER UUDT fallback
/// instead. Requires SAE J2534-2 clause 5 opt-in (a `"J2534-2:"` `pname`
/// prefix), same as `fd_iso15765.rs`'s own FD-substitution tests.
#[tokio::test]
#[serial]
async fn native_mixed_mode_excludes_fd_substituted_link() {
    let extra = format!(
        "can_channel_mode = \"native-mixed\"\n{}",
        modules_toml(&[("Bench 1", "J2534-2:mock")])
    );
    let server = TestServer::try_start_with_extra_config(&extra)
        .await
        .expect("service should initialize with native-mixed mode and a J2534-2-opted-in module");
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_ISO15765_PS,
        "staged FD ComParams should still substitute to FD_ISO15765_PS under native-mixed"
    );
    assert!(
        !server
            .backdoor
            .set_config_param_log(MOCK_CHANNEL_ID)
            .contains(&j2534_0404::CONFIG_CAN_MIXED_FORMAT),
        "an FD-substituted link must never get CAN_MIXED_FORMAT enabled (ADR-160's explicit \
         FD exclusion) -- it keeps ADR-159's existing FLOW_CONTROL_FILTER UUDT fallback"
    );

    server.shutdown().await;
}

/// Finding 1 (Codex review, PR #32): `ISO15765_ADDR_TYPE` (SAE J2534-1 Table
/// B.13's extended-addressing indicator) is ISO15765-protocol-specific and
/// must never leak onto a native-mixed `PASS_FILTER`'s raw-CAN messages, even
/// when the UUDT entry's `CP_CanRespUUDTFormat` sets the extended-addressing
/// bit. The extension-address payload byte itself (a plain CAN data byte,
/// valid on any protocol) is unaffected and must still be appended.
#[tokio::test]
#[serial]
async fn native_mixed_mode_pass_filter_uudt_extended_addressing_excludes_iso15765_addr_type() {
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
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_RESP_UUDT_FORMAT, can_id_format::EXTENDED_11BIT),
                unum32_param(CP_CAN_RESP_UUDT_EXT_ADDR, 0xF1),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        j2534_0404::PASS_FILTER
    );

    let expected_mask = vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    let expected_pattern = {
        let mut bytes = 0x5E8_u32.to_be_bytes().to_vec();
        bytes.push(0xF1);
        bytes
    };
    assert_eq!(
        server.backdoor.filter_mask(MOCK_CHANNEL_ID, 1),
        expected_mask,
        "the extension-address mask byte must still be appended (5 bytes, not 4)"
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 1),
        expected_pattern,
        "the extension-address payload byte must still be appended (5 bytes, not 4)"
    );
    assert_eq!(
        server.backdoor.filter_pattern_tx_flags(MOCK_CHANNEL_ID, 1),
        0,
        "ISO15765_ADDR_TYPE is ISO15765-only and must never be set on a raw-CAN PASS_FILTER, \
         even for an extended-addressed UUDT entry"
    );

    server.shutdown().await;
}

/// Finding 2 (Codex review, PR #32): a `UniqueRespIdTable` entry that only
/// carries `CP_CanRespUUDTId` (no `CP_CanPhysReqId` at all -- a valid
/// receive-only `SetUniqueRespIdTable` input) must still get a native-mixed
/// `PASS_FILTER`, since that filter type has no `pFlowControlMsg` and never
/// reads the request address.
#[tokio::test]
#[serial]
async fn native_mixed_mode_installs_pass_filter_for_uudt_only_entry_without_req_id() {
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
        vec![ecu_entry(3, vec![unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8)])],
    )
    .await;

    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "a UUDT-only entry (no CP_CanPhysReqId) must still get a native-mixed PASS_FILTER"
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::PASS_FILTER
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        0x5E8_u32.to_be_bytes()
    );
    assert_eq!(
        server.backdoor.filter_flow_control(MOCK_CHANNEL_ID, 0),
        None,
        "a PASS_FILTER has no pFlowControlMsg"
    );

    server.shutdown().await;
}

/// Counterpart to the test above: outside native-mixed mode, the UUDT
/// fallback filter is a `FLOW_CONTROL_FILTER`, which still genuinely
/// requires a request address -- a UUDT-only entry with no
/// `CP_CanPhysReqId` correctly gets no filter at all under this (pre-existing)
/// mode, confirming Finding 2's fix is scoped to the native-mixed
/// `PASS_FILTER` path only.
#[tokio::test]
#[serial]
async fn non_native_mixed_mode_skips_uudt_only_entry_without_req_id() {
    let server = TestServer::start_with_can_mode(Some("single-channel")).await;
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
        vec![ecu_entry(3, vec![unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8)])],
    )
    .await;

    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        0,
        "a UUDT-only entry with no CP_CanPhysReqId still requires a request address for the \
         FLOW_CONTROL_FILTER fallback outside native-mixed mode"
    );

    server.shutdown().await;
}

/// `install_point_to_point_fc_filters`' `qualified` exclusion: a pin-selected
/// (SAE J2534-2 clause 6 Pin Selection) link never gets the native-mixed
/// PASS_FILTER treatment for its UUDT id -- it keeps the ADR-041
/// FLOW_CONTROL_FILTER fallback instead, exactly like `dual_channel`'s own
/// `qualified` exclusion. ADR-160 Correction (2026-08-17, part 2): the
/// channel's own connect-time `SET_CONFIG(CAN_MIXED_FORMAT, ON)` is no
/// longer issued at all for a qualified link (previously it still fired,
/// after `SET_CONFIG(CONFIG_J1962_PINS)` per the part-1 ordering
/// correction, only the UUDT filter mechanism differed -- but leaving it on
/// exposed a clause 8 Figure 1 hazard once a qualified link's connect could
/// actually succeed). Uses CAN's own clause 6.3.1 example pins (3/HI,
/// 11/LOW, differing from ISO15765's default 6/14), matching
/// `pin_selection.rs`'s own non-default pin convention.
#[tokio::test]
#[serial]
async fn native_mixed_mode_qualified_link_still_uses_fc_filter_fallback() {
    let extra = format!(
        "can_channel_mode = \"native-mixed\"\n{}",
        modules_toml(&[("Bench 1", "J2534-2:mock")])
    );
    let server = TestServer::try_start_with_extra_config(&extra)
        .await
        .expect("service should initialize with native-mixed mode and a J2534-2-opted-in module");
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![
                        PinData {
                            dlc_pin_number: 3,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "HI".to_string(),
                                ),
                            ),
                        },
                        PinData {
                            dlc_pin_number: 11,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "LOW".to_string(),
                                ),
                            ),
                        },
                    ],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolId(j2534_0404::ISO15765)),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed for a well-formed Pin Selection request")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for a pin-selected ISO15765_PS link");

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        j2534_0404::FLOW_CONTROL_FILTER,
        "a pin-selected (qualified) link must keep the ADR-041 FLOW_CONTROL_FILTER fallback for \
         its UUDT id, never the native-mixed PASS_FILTER"
    );
    assert_eq!(
        server
            .backdoor
            .filter_pattern_protocol_id(MOCK_CHANNEL_ID, 1),
        j2534_0404::PROTOCOL_ISO15765_PS,
        "the qualified-link FLOW_CONTROL_FILTER fallback must keep the channel's own literal \
         connect-time ProtocolID -- the native-mixed PASS_FILTER's raw-CAN substitution \
         (resources::mixed_format_can_protocol_id) must never leak into this path"
    );
    assert!(
        !server
            .backdoor
            .set_config_param_log(MOCK_CHANNEL_ID)
            .contains(&j2534_0404::CONFIG_CAN_MIXED_FORMAT),
        "ADR-160 Correction (2026-08-17, part 2): a pin-selected/qualified link's physical \
         channel must never have CONFIG_CAN_MIXED_FORMAT SET_CONFIG'd at all -- leaving it on \
         while the link keeps the FLOW_CONTROL_FILTER fallback (asserted above) would expose \
         the clause 8 Figure 1 UUDT misparse/loss hazard"
    );

    server.shutdown().await;
}

/// ADR-160 Correction (2026-08-17, part 2) regression: on a pin-selected
/// `ISO15765_PS` link under native-mixed mode, `connect_new_physical_channel`
/// must never `SET_CONFIG(CONFIG_CAN_MIXED_FORMAT)` at all -- superseding
/// this test's original purpose (pinning that, when it WAS issued, it came
/// after `SET_CONFIG(CONFIG_J1962_PINS)` per clause 6.3.2.7's pin-first
/// rule), which the part-2 correction moots by removing the `SET_CONFIG`
/// for a qualified link outright. `CONFIG_J1962_PINS` itself is still
/// asserted to confirm pins are applied regardless.
#[tokio::test]
#[serial]
async fn native_mixed_mode_qualified_link_skips_mixed_format_set_config() {
    let extra = format!(
        "can_channel_mode = \"native-mixed\"\n{}",
        modules_toml(&[("Bench 1", "J2534-2:mock")])
    );
    let server = TestServer::try_start_with_extra_config(&extra)
        .await
        .expect("service should initialize with native-mixed mode and a J2534-2-opted-in module");
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle { module_handle: 1 }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![
                        PinData {
                            dlc_pin_number: 3,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "HI".to_string(),
                                ),
                            ),
                        },
                        PinData {
                            dlc_pin_number: 11,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "LOW".to_string(),
                                ),
                            ),
                        },
                    ],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolId(j2534_0404::ISO15765)),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed for a well-formed Pin Selection request")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for a pin-selected ISO15765_PS link");

    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "CONFIG_J1962_PINS should have been SET_CONFIG'd; log was {log:#x?}"
    );
    assert!(
        !log.contains(&j2534_0404::CONFIG_CAN_MIXED_FORMAT),
        "ADR-160 Correction (2026-08-17, part 2): CONFIG_CAN_MIXED_FORMAT must never be \
         SET_CONFIG'd on a pin-selected/qualified link's physical channel; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Same regression as
/// `native_mixed_mode_qualified_link_skips_mixed_format_set_config` above,
/// but for an SW-family (`SW_ISO15765_PS`, ADR-164)
/// qualified link instead of a plain `ISO15765_PS` one, confirming the
/// ADR-160 Correction (2026-08-17, part 2) `SET_CONFIG` skip covers the
/// SW-family arm too, not just the plain `ISO15765_PS` case -- an SW link
/// reaches the same `base_protocol_id() == PROTOCOL_ISO15765` gate as a
/// plain qualified link (via `is_sw_protocol`, separately from
/// `is_ps_protocol`'s narrower 7-entry table, the exact gap the mock's own
/// clause 6.3.2.7 pin-gate previously missed for part 1's ordering fix), so
/// this test guards that the part-2 exclusion (`link_pin_select.is_none()
/// && link_channel_index.is_none()`) generalizes across both families
/// rather than accidentally being scoped to `is_ps_protocol`. Connects via
/// the SWCAN sibling resource row (`resources.rs` 0x022B, `ISO_15765_2_
/// SWCAN`, `hw_protocol_override = PROTOCOL_SW_ISO15765_PS`) with no
/// explicit `dlc_pin_data`, matching `sw_can.rs`'s own
/// `connecting_an_sw_resource_emits_the_internal_pins_set_config` -- clause
/// 9.2.1 defines no implicit default connect for SW at all, so
/// `resolve_pin_selection`'s SW arm always supplies a real `pin_select`
/// (the row's own single default pin, 1/HI) internally, never `Ok(None)`.
#[tokio::test]
#[serial]
async fn native_mixed_mode_qualified_sw_link_skips_mixed_format_set_config() {
    const SW_ISO15765_RESOURCE_ID: u32 = 0x022B;
    const SWCAN_BAUD_RATE: u32 = 33_300;

    let extra = format!(
        "can_channel_mode = \"native-mixed\"\n{}",
        modules_toml(&[("Bench 1", "J2534-2:mock")])
    );
    let server = TestServer::try_start_with_extra_config(&extra)
        .await
        .expect("service should initialize with native-mixed mode and a J2534-2-opted-in module");
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, SW_ISO15765_RESOURCE_ID, 1).await;
    set_com_param_unum32(
        &mut client,
        cll_handle,
        j2534_0404::DATA_RATE,
        SWCAN_BAUD_RATE,
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for an SW_ISO15765_PS-resolved link");

    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "CONFIG_J1962_PINS should have been SET_CONFIG'd; log was {log:#x?}"
    );
    assert!(
        !log.contains(&j2534_0404::CONFIG_CAN_MIXED_FORMAT),
        "ADR-160 Correction (2026-08-17, part 2): CONFIG_CAN_MIXED_FORMAT must never be \
         SET_CONFIG'd on an SW-family qualified link's physical channel either -- the exclusion \
         must not be accidentally scoped to only the plain ISO15765_PS case; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Same regression as
/// `native_mixed_mode_qualified_link_skips_mixed_format_set_config` above,
/// but for a directly-named `_CHx` (Additional Channels, ADR-156 Decision
/// 3/ADR-178) qualified link instead of a pin-selected `_PS` one --
/// confirming the ADR-160 Correction (2026-08-17, part 2) exclusion
/// (`link_pin_select.is_none() && link_channel_index.is_none()`) actually
/// covers its `link_channel_index` half, not just `link_pin_select`.
/// edge-case-hunter (round 3) found every prior test in this file only
/// exercised the `_PS` half of that conjunction -- dropping the
/// `link_channel_index.is_none()` term from the exclusion left the whole
/// suite passing unchanged, a live coverage gap. Builds the `_CHx` link the
/// same way `additional_channels.rs` does post-ADR-178: naming the raw
/// `_CHx` hardware protocol id directly via `ResourceData.protocol_id`
/// (`harness.rs`'s `create_cll` already wraps this route), not the removed
/// `channel_index` field.
#[tokio::test]
#[serial]
async fn native_mixed_mode_chx_qualified_link_skips_mixed_format_set_config() {
    let extra = format!(
        "can_channel_mode = \"native-mixed\"\n{}",
        modules_toml(&[("Bench 1", "J2534-2:mock")])
    );
    let server = TestServer::try_start_with_extra_config(&extra)
        .await
        .expect("service should initialize with native-mixed mode and a J2534-2-opted-in module");
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::PROTOCOL_ISO15765_CH1, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for a directly-named _CH1 ISO15765 link");

    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        !log.contains(&j2534_0404::CONFIG_CAN_MIXED_FORMAT),
        "ADR-160 Correction (2026-08-17, part 2): CONFIG_CAN_MIXED_FORMAT must never be \
         SET_CONFIG'd on a _CHx-qualified link's physical channel either -- the exclusion must \
         cover link_channel_index, not just link_pin_select; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// ADR-162 Decision 2, fail-fast self-check: a single CLL's own
/// `UniqueRespIdTable` with an entry that sets `CP_CanRespUSDTId` and
/// `CP_CanRespUUDTId` to the identical (flow-control-eligible) match key
/// must reject `ConnectComLogicalLink` under native-mixed mode, before any
/// physical channel is created.
#[tokio::test]
#[serial]
async fn native_mixed_mode_connect_self_collision_is_rejected() {
    let server = TestServer::start_with_can_mode(Some("native-mixed")).await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "connect should reject a UniqueRespIdTable whose UUDT and flow-control-eligible \
             USDT match keys collide under native-mixed mode",
        );

    assert_eq!(status.code(), Code::FailedPrecondition);
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrFctFailed as i32);

    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "no physical channel should have been created for a self-colliding table"
    );

    server.shutdown().await;
}

/// ADR-162 Decision 2, authoritative cross-CLL check inside
/// `finalize_connected_link`: CLL A connects first and establishes a UUDT
/// id on the shared physical channel; CLL B then tries to join the same
/// channel with a flow-control-eligible USDT id that collides with A's
/// UUDT id. B's connect must be rejected, and the join's `ref_count`/
/// `occupancy_epoch` bump must be rolled back (ADR-161) -- verified
/// behaviorally, since `SharedChannel` state is not reachable from this
/// black-box test: disconnecting A afterward (the channel's only surviving
/// occupant) must tear down the physical channel exactly once. If B's
/// `ref_count` bump had leaked, the channel would still show two occupants
/// and disconnecting A alone would not reach zero.
#[tokio::test]
#[serial]
async fn native_mixed_mode_connect_cross_cll_collision_is_rejected_and_rolled_back() {
    let server = TestServer::start_with_can_mode(Some("native-mixed")).await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_a,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);

    // CLL B: same (ISO15765, 500kbps) physical channel key as A, so it joins
    // A's already-open channel -- staged with a USDT flow-control-eligible
    // id that collides with A's already-active UUDT id.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_unique_resp_table(
        &mut client,
        cll_b,
        vec![ecu_entry(
            9,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7F0),
            ],
        )],
    )
    .await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect_err(
            "connect should reject a joining CLL's USDT match key that collides with a \
             sibling's already-active UUDT match key on the same shared native-mixed channel",
        );
    assert_eq!(status.code(), Code::FailedPrecondition);
    let detail = error_detail_from_status(&status).expect("rejection should carry an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrFctFailed as i32);

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "B's rejected join must not have issued a second PassThruConnect"
    );

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link(A) should succeed");

    assert_eq!(
        server.backdoor.disconnect_count(),
        1,
        "A was the channel's only surviving occupant after B's rejected join was rolled back -- \
         disconnecting A alone must tear down the physical channel"
    );

    server.shutdown().await;
}

/// ADR-162 Decision 2, promote-time enforcement: a `CoptUpdateparam` that
/// would promote a colliding table is rejected -- the old table and filters
/// stay installed, a `PDU_ERR_EVT_PROT_ERR` error event is observed, and the
/// COP still reaches `PduCopstFinished` (rejecting the table promotion does
/// not fail the COP, since `PduComPrimitiveStatus` has no failed variant).
#[tokio::test]
#[serial]
async fn native_mixed_mode_promote_collision_is_rejected_old_filters_stay_installed() {
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
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);
    let filter_type_0_before = server.backdoor.filter_type(MOCK_CHANNEL_ID, 0);
    let filter_type_1_before = server.backdoor.filter_type(MOCK_CHANNEL_ID, 1);
    let filter_pattern_1_before = server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 1);

    // Stage a colliding table (self-collision: USDT and UUDT ids equal) and
    // issue CoptUpdateparam directly -- subscribed BEFORE starting it so the
    // error event cannot be missed.
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x5E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    let mut events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptUpdateparam as i32,
            cop_data: Vec::new(),
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptUpdateparam) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, is_prot_err_error).await,
        "expected a PduErrEvtProtErr error event for the rejected colliding table"
    );
    wait_for_cop_finished(&mut events).await;

    drop(events);

    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        2,
        "the rejected promote must not have torn down or reinstalled any filters"
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        filter_type_0_before
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        filter_type_1_before
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 1),
        filter_pattern_1_before,
        "the old UUDT PASS_FILTER's pattern must be unchanged"
    );

    server.shutdown().await;
}

/// ADR-162 Decision 2 acceptance case: a `CP_CanRespUSDTId` sharing a
/// numeric value with a `CP_CanRespUUDTId`, but with flow control disabled,
/// never gets a `FLOW_CONTROL_FILTER` installed and so cannot collide --
/// connect must succeed normally under native-mixed mode.
#[tokio::test]
#[serial]
async fn native_mixed_mode_accepts_flow_control_disabled_usdt_sharing_uudt_numeric_id() {
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
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x5E8),
                unum32_param(CP_CAN_RESP_USDT_FORMAT, can_id_format::NORMAL_FC_DISABLED),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "only the UUDT PASS_FILTER should install -- the flow-control-disabled USDT id never \
         gets a FLOW_CONTROL_FILTER and so cannot collide with it"
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::PASS_FILTER
    );

    server.shutdown().await;
}

/// ADR-162 Decision 2 acceptance case: a `CP_CanRespUSDTId` sharing a
/// numeric value with a `CP_CanRespUUDTId`, but with no `CP_CanPhysReqId`,
/// never gets a `FLOW_CONTROL_FILTER` installed either -- connect must
/// succeed normally under native-mixed mode.
#[tokio::test]
#[serial]
async fn native_mixed_mode_accepts_usdt_with_no_phys_req_id_sharing_uudt_numeric_id() {
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
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x5E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
            ],
        )],
    )
    .await;

    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        1,
        "only the UUDT PASS_FILTER should install -- a USDT id with no CP_CanPhysReqId never \
         gets a FLOW_CONTROL_FILTER and so cannot collide with it"
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::PASS_FILTER
    );

    server.shutdown().await;
}

/// ADR-162 Decision 2 acceptance case: the identical colliding table that
/// `native_mixed_mode_connect_self_collision_is_rejected` rejects must
/// connect -- and later promote to a different colliding table -- normally
/// under a non-native-mixed `CanChannelMode`, since without a native-mixed
/// `PASS_FILTER` installed there is nothing for the collision to break.
#[tokio::test]
#[serial]
async fn non_native_mixed_mode_accepts_a_colliding_uudt_usdt_table() {
    let server = TestServer::start_with_can_mode(Some("single-channel")).await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x7E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "the identical colliding table must connect normally outside native-mixed mode -- \
             without a native-mixed PASS_FILTER installed for it, there is nothing for the \
             collision to break",
        );

    // Confirm this is still exercising a collision-shaped table (both a
    // FLOW_CONTROL_FILTER-eligible USDT id and a UUDT id at the same
    // numeric address), not an accidentally-empty one: the pre-existing
    // ADR-041 workaround installs a FLOW_CONTROL_FILTER for each.
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    // The promote-time path must likewise accept a (different) colliding
    // table outside native-mixed mode.
    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            5,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x5E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    server.shutdown().await;
}

// ---------------------------------------------------------------------
// `can_channel_mode = "native-mixed-all-frames"` (ADR-217): everything
// above this point exercises `"native-mixed"` (`CAN_MIXED_FORMAT_ON`)
// only, and stays in force unchanged -- see
// `native_mixed_mode_connect_self_collision_is_rejected` above in
// particular, which is this file's existing ADR-162 regression guard that
// the collision check stays `ON`-only (confirmed still passing by the full
// crate test run in this PR, not duplicated below).
// ---------------------------------------------------------------------

/// ADR-217 Decision item 4: an ISO15765-family CLL connecting under
/// `"native-mixed-all-frames"` issues `SET_CONFIG(CAN_MIXED_FORMAT,
/// ALL_FRAMES)` (value `2`) once, at physical-channel creation -- not value
/// `1` (`ON`), mirroring `native_mixed_mode_enables_can_mixed_format_at_
/// connect`'s own shape for the ON case above.
#[tokio::test]
#[serial]
async fn native_mixed_all_frames_mode_enables_can_mixed_format_all_frames_at_connect() {
    let server = TestServer::start_with_can_mode(Some("native-mixed-all-frames")).await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_CAN_MIXED_FORMAT),
        2,
        "native-mixed-all-frames mode should SET_CONFIG(CAN_MIXED_FORMAT, ALL_FRAMES = 2) at \
         connect, not ON (1)"
    );
    assert!(
        server
            .backdoor
            .set_config_param_log(MOCK_CHANNEL_ID)
            .contains(&j2534_0404::CONFIG_CAN_MIXED_FORMAT),
        "CONFIG_CAN_MIXED_FORMAT should appear in the connect-time SET_CONFIG log"
    );

    server.shutdown().await;
}

/// ADR-217's actual motivating scenario: the identical overlapping
/// `CP_CanRespUUDTId`/flow-control-eligible `CP_CanRespUSDTId` table that
/// `native_mixed_mode_connect_self_collision_is_rejected` (above) rejects
/// under plain `"native-mixed"` must instead be SERVABLE under
/// `"native-mixed-all-frames"` -- staged the same way (BEFORE `Connect`, so
/// this exercises the connect-time fail-fast self-check, the actual
/// call site ADR-217 relaxes for this sub-mode -- not
/// `promote_unique_resp_id_table`'s own independent, still-ON-only gate,
/// which a post-connect `SetUniqueRespIdTable`/`CoptUpdateparam` staging
/// would exercise instead and prove nothing about this change). `Connect`
/// must succeed, and both filters (the USDT `FLOW_CONTROL_FILTER` and the
/// UUDT `PASS_FILTER`) must install, proving the ADR-162 collision check
/// does not fire for this sub-mode (ADR-217 Decision item 3: the collision
/// check stays `ON`-only).
#[tokio::test]
#[serial]
async fn native_mixed_all_frames_mode_serves_the_uudt_usdt_collision_table() {
    let server = TestServer::start_with_can_mode(Some("native-mixed-all-frames")).await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    // Same collision shape as `native_mixed_mode_connect_self_collision_is_
    // rejected`: CP_CanRespUSDTId and CP_CanRespUUDTId set to the identical
    // flow-control-eligible match key, staged before Connect.
    set_unique_resp_table(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x5E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "connect must succeed under native-mixed-all-frames for a table that would be \
             rejected under plain native-mixed -- ADR-217's collision-becomes-servable claim",
        );

    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        2,
        "the overlapping table must install BOTH filters under all-frames mode -- 0 or 1 \
         filter would mean the table was rejected or partially applied, contradicting ADR-217's \
         collision-becomes-servable claim"
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 0),
        j2534_0404::FLOW_CONTROL_FILTER,
        "USDT keeps the conformant FLOW_CONTROL_FILTER"
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 0),
        0x5E8_u32.to_be_bytes()
    );
    assert_eq!(
        server.backdoor.filter_type(MOCK_CHANNEL_ID, 1),
        j2534_0404::PASS_FILTER,
        "UUDT still gets its own PASS_FILTER, not rejected by the ADR-162 collision check"
    );
    assert_eq!(
        server.backdoor.filter_pattern(MOCK_CHANNEL_ID, 1),
        0x5E8_u32.to_be_bytes()
    );

    server.shutdown().await;
}

/// ADR-217's dual-delivery claim, exercised end-to-end (Decision item 5: no
/// RX dispatch change was needed because the existing per-message
/// `ProtocolID` branch already handles this). With the overlapping table
/// from the test above staged and active, a matching frame delivered twice
/// by the device -- once tagged `ISO15765` (a `FLOW_CONTROL_FILTER` match,
/// reassembled), once tagged `CAN` (a `PASS_FILTER` match, raw) -- must
/// reach the client as TWO separate `ResultData` events, both carrying this
/// entry's `unique_resp_identifier`.
#[tokio::test]
#[serial]
async fn native_mixed_all_frames_mode_delivers_both_usdt_and_uudt_interpretations() {
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
                unum32_param(CP_CAN_RESP_USDT_ID, 0x5E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 2);

    arm_receive_only_monitor(&mut client, cll_handle).await;

    // The USDT/FLOW_CONTROL_FILTER interpretation: the device's own ISO-TP
    // reassembly already produced the complete message, delivered natively
    // tagged ISO15765 -- same shape as
    // `native_mixed_mode_iso15765_tagged_frames_still_route_via_usdt_path`.
    let usdt_payload = vec![0x62, 0xF1, 0x90, 0xAA];
    let usdt_frame = can_frame(0x5E8, &usdt_payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &usdt_frame, j2534_0404::ISO15765);

    // The UUDT/PASS_FILTER interpretation of the SAME logical frame: a raw
    // CAN-tagged delivery, same CAN ID, its own (unrelated) payload -- same
    // shape as `native_mixed_mode_routes_can_tagged_frames_via_uudt_only`.
    let uudt_payload = vec![0xAA, 0xBB, 0xCC, 0xDD];
    let uudt_frame = can_frame(0x5E8, &uudt_payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &uudt_frame, j2534_0404::CAN);

    let mut seen_usdt = false;
    let mut seen_uudt = false;
    for _ in 0..2 {
        let result = wait_for_result_data(&mut client, cll_handle).await;
        assert_eq!(
            result.unique_resp_identifier, 3,
            "both deliveries come from the same UniqueRespIdTable entry"
        );
        if result.data_bytes == usdt_payload {
            seen_usdt = true;
        } else if result.data_bytes == uudt_payload {
            seen_uudt = true;
        } else {
            panic!("unexpected ResultData payload: {:?}", result.data_bytes);
        }
    }
    assert!(
        seen_usdt,
        "the ISO15765-tagged (USDT) interpretation must have been delivered"
    );
    assert!(
        seen_uudt,
        "the CAN-tagged (UUDT) interpretation must have been delivered too -- this is ADR-217's \
         dual-delivery claim, not just the collision check being relaxed"
    );

    server.shutdown().await;
}

/// ADR-217 Decision item 7 / Consequences: `"native-mixed"` (`ON`) is
/// unaffected by this change -- an ordinary, non-overlapping round-trip
/// still behaves exactly as before. A light spot-check: this file's
/// existing `native-mixed` suite above (in particular
/// `native_mixed_mode_routes_can_tagged_frames_via_uudt_only` and
/// `native_mixed_mode_enables_can_mixed_format_at_connect`) already
/// provides thorough coverage, so this only re-confirms the connect-time
/// `SET_CONFIG` value stays `1` (not `2`) and an ordinary UUDT round-trip
/// still delivers on this same suite, unchanged by `NativeMixedAllFrames`
/// existing as a sibling variant.
#[tokio::test]
#[serial]
async fn native_mixed_mode_still_sends_on_not_all_frames_after_all_frames_variant_added() {
    let server = TestServer::start_with_can_mode(Some("native-mixed")).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_CAN_MIXED_FORMAT),
        1,
        "native-mixed mode must still send CAN_MIXED_FORMAT_ON (1), not ALL_FRAMES (2)"
    );

    set_unique_resp_table_and_promote(
        &mut client,
        cll_handle,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    let uudt_payload = vec![0x62, 0xF1, 0x90, 0xAA];
    let uudt_frame = can_frame(0x5E8, &uudt_payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &uudt_frame, j2534_0404::CAN);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x5E8_u32.to_be_bytes(), &[], &uudt_payload);
    assert_eq!(result.unique_resp_identifier, 3);

    server.shutdown().await;
}

/// `edge-case-hunter` finding, this ADR's own review: the collision test
/// above (`native_mixed_all_frames_mode_serves_the_uudt_usdt_collision_table`)
/// only exercises the SELF-collision fail-fast path (a single CLL's own
/// table checked against itself, `rpc_connect_com_logical_link`'s own
/// early-return check) -- structurally different from the cross-CLL scan
/// `finalize_connected_link` runs (`enforce_native_mixed_collision`, fed by
/// the SAME shared `native_mixed_collision_enforced` boolean, but a
/// different call site entirely: it inspects a SIBLING CLL's already-active
/// table, which the self-check never sees). Mirrors
/// `native_mixed_mode_connect_cross_cll_collision_is_rejected_and_rolled_back`
/// above exactly, but under `"native-mixed-all-frames"`: CLL B joining CLL
/// A's already-open channel with a USDT key that collides with A's active
/// UUDT key must SUCCEED (not be rejected), proving the cross-CLL check --
/// not just the self-check -- correctly skips ALL_FRAMES too.
#[tokio::test]
#[serial]
async fn native_mixed_all_frames_mode_serves_a_cross_cll_uudt_usdt_collision() {
    let server = TestServer::start_with_can_mode(Some("native-mixed-all-frames")).await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    set_unique_resp_table_and_promote(
        &mut client,
        cll_a,
        vec![ecu_entry(
            3,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x7E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);

    // CLL B: same (ISO15765, 500kbps) physical channel key as A, so it joins
    // A's already-open channel -- staged with a USDT flow-control-eligible
    // id that collides with A's already-active UUDT id. Under plain
    // "native-mixed" this join is rejected (see the ON-mode test above);
    // under "native-mixed-all-frames" it must succeed instead.
    let cll_b = create_cll(&mut client, j2534_0404::ISO15765, 2).await;
    set_com_param_unum32(&mut client, cll_b, j2534_0404::DATA_RATE, 500_000).await;
    set_unique_resp_table(
        &mut client,
        cll_b,
        vec![ecu_entry(
            9,
            vec![
                unum32_param(CP_CAN_RESP_USDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7F0),
            ],
        )],
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect(
            "under native-mixed-all-frames, a joining CLL's USDT match key colliding with a \
             sibling's already-active UUDT match key on the same shared channel must be \
             servable, not rejected -- the cross-CLL collision scan must skip this sub-mode \
             exactly like the self-collision check does",
        );

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "B should join A's existing channel, not open a second physical channel"
    );
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        3,
        "A's FLOW_CONTROL_FILTER + PASS_FILTER, plus B's own new FLOW_CONTROL_FILTER for 0x5E8 \
         -- all three installed, none rejected"
    );

    // ADR-217 Codex-review fix, PR #132 round 2: the collision above being
    // ACCEPTED at connect time is not the whole story -- the two deliveries
    // it produces must also reach the RIGHT CLL, not fan out to both. A's own
    // table entry has `CP_CanRespUUDTId = 0x5E8` (no USDT at that id); B's has
    // `CP_CanRespUSDTId = 0x5E8` (no UUDT at all). Before this fix,
    // `route_frame`'s role-agnostic UUDT-tier matching let A's own entry also
    // claim an ISO15765-tagged delivery for 0x5E8 that genuinely belongs to
    // B's USDT interpretation.
    arm_receive_only_monitor(&mut client, cll_a).await;
    arm_receive_only_monitor(&mut client, cll_b).await;

    // The ISO15765-tagged (USDT) interpretation of 0x5E8 is B's own genuine
    // delivery -- A's colliding UUDT entry for the same CAN ID must NOT also
    // claim it.
    let usdt_payload = vec![0x62, 0xF1, 0x90];
    let usdt_frame = can_frame(0x5E8, &usdt_payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &usdt_frame, j2534_0404::ISO15765);
    let result = wait_for_result_data(&mut client, cll_b).await;
    assert_eq!(
        result.unique_resp_identifier, 9,
        "the ISO-tagged delivery for 0x5E8 belongs to B's USDT entry"
    );
    assert_eq!(result.data_bytes, usdt_payload);
    assert_no_result_data(
        &mut client,
        cll_a,
        "A's own UUDT=0x5E8 entry must NOT also claim an ISO-tagged (USDT) delivery for the \
         same CAN ID -- that delivery belongs exclusively to B",
    )
    .await;

    // The CAN-tagged (UUDT) interpretation of 0x5E8 is A's own genuine
    // delivery via its PASS_FILTER -- B (no UUDT field at all) must not
    // receive it either.
    let uudt_payload = vec![0xAA, 0xBB, 0xCC, 0xDD];
    let uudt_frame = can_frame(0x5E8, &uudt_payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &uudt_frame, j2534_0404::CAN);
    let result = wait_for_result_data(&mut client, cll_a).await;
    assert_eq!(
        result.unique_resp_identifier, 3,
        "the CAN-tagged delivery for 0x5E8 belongs to A's UUDT entry"
    );
    assert_eq!(result.data_bytes, uudt_payload);
    assert_no_result_data(
        &mut client,
        cll_b,
        "B has no CP_CanRespUUDTId at all -- it must not receive the CAN-tagged delivery",
    )
    .await;

    server.shutdown().await;
}

/// ADR-217 Decision item 5 (Codex review, PR #132): a row whose
/// `CP_CanRespUSDTId` is extended-addressed but whose `CP_CanRespUUDTId` on
/// the SAME CAN ID is normal-addressed must not have the USDT entry's
/// `Addressing::Extended` bleed into the UUDT-routed (CAN-tagged) delivery's
/// own header split. Before the role-split fix, `header_footer_len`'s
/// `.any()` lookup against one flat `can_addressing_by_id` table would see
/// the USDT entry's `Extended` and wrongly widen this CAN-tagged delivery to
/// a 5-byte header too, stripping its first real payload byte as a phantom
/// Address Extension byte.
#[tokio::test]
#[serial]
async fn native_mixed_all_frames_mode_uudt_delivery_keeps_its_own_normal_addressing() {
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
                unum32_param(CP_CAN_RESP_USDT_ID, 0x5E8),
                unum32_param(CP_CAN_RESP_USDT_FORMAT, can_id_format::EXTENDED_11BIT),
                unum32_param(CP_CAN_RESP_USDT_EXT_ADDR, 0xF1),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    // A CAN-tagged (UUDT-routed) delivery for the colliding CAN ID -- the
    // UUDT entry above sets no extended-addressing ComParams, so this must
    // stay a plain 4-byte-headered delivery even though the USDT entry for
    // the SAME numeric CAN ID is extended.
    let payload = vec![0xAA, 0xBB, 0xCC, 0xDD];
    let frame = can_frame(0x5E8, &payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::CAN);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x5E8_u32.to_be_bytes(), &[], &payload);
    assert_eq!(result.unique_resp_identifier, 3);

    server.shutdown().await;
}

/// ADR-217 Decision item 5 (Codex review, PR #132): the mirror image of the
/// test above -- `CP_CanRespUSDTId` normal-addressed, `CP_CanRespUUDTId` on
/// the same CAN ID extended-addressed. The USDT-routed (ISO15765-tagged)
/// delivery must NOT be widened by the UUDT entry's `Addressing::Extended`,
/// or the reassembled response's own first payload byte (its SID) is wrongly
/// stripped as a phantom Address Extension byte.
#[tokio::test]
#[serial]
async fn native_mixed_all_frames_mode_usdt_delivery_keeps_its_own_normal_addressing() {
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
                unum32_param(CP_CAN_RESP_USDT_ID, 0x5E8),
                unum32_param(CP_CAN_RESP_UUDT_ID, 0x5E8),
                unum32_param(CP_CAN_RESP_UUDT_FORMAT, can_id_format::EXTENDED_11BIT),
                unum32_param(CP_CAN_RESP_UUDT_EXT_ADDR, 0xF1),
                unum32_param(CP_CAN_PHYS_REQ_ID, 0x7E0),
            ],
        )],
    )
    .await;

    arm_receive_only_monitor(&mut client, cll_handle).await;

    // An ISO15765-tagged (USDT-routed) delivery for the colliding CAN ID --
    // native RxStatus bit 7 clear (plain `inject_rx`), and the USDT entry
    // above sets no extended-addressing ComParams, so this must stay a plain
    // 4-byte-headered delivery even though the UUDT entry for the SAME
    // numeric CAN ID is extended.
    let payload = vec![0x62, 0xF1, 0x90];
    let frame = can_frame(0x5E8, &payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::ISO15765);

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x5E8_u32.to_be_bytes(), &[], &payload);
    assert_eq!(result.unique_resp_identifier, 3);

    server.shutdown().await;
}

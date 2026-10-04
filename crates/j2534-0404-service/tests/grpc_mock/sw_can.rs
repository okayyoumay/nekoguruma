//! End-to-end coverage for SAE J2534-2 clause 9 Single Wire CAN (SWCAN/GMLAN,
//! ADR-164, Phase 4): connecting an SW resource row's mandatory internal
//! `SET_CONFIG(CONFIG_J1962_PINS)` (clause 9.2.1's "no implicit default"
//! model), the `CP_ChangeSpeed*` ComParam family's accept-all/translate-3
//! scope (ADR-164 Decision 2), `CP_SwCan_HighVoltage`'s `TX_FLAG_SW_CAN_HV_TX`
//! wiring gated on the link's SW hw id (Consequences), `SW_CAN_HS`/`SW_CAN_NS`
//! IOCTL exposure (Decision 3), and the SWCAN-vs-CAN-FD mutual-exclusion
//! guard `rpc_link.rs` added to `apply_fd_mode`. Mirrors `fd_can.rs`'s/
//! `fd_iso15765.rs`'s structure and helpers wherever the underlying mechanism
//! is shared (many helpers live in `harness.rs` and are reused here, not
//! duplicated).
//!
//! Item 9 of this file's originating brief (a pre-existing bare, unsuffixed
//! `protocol_name` like `"ISO_15765_2"` still resolving unambiguously with no
//! pin data) is not duplicated here: it is already covered by
//! `names.rs`'s own unit tests
//! (`parse_protocol_id_from_resource_extends_channel_selection_to_a_table_row_name`,
//! `resolve_object_id_objt_resource_resolves_through_table_first`,
//! `resolve_resource_ids_from_data_filters_by_protocol_name`), which are the
//! exact regression tests ADR-164's naming correction (distinct `_SWCAN`
//! -suffixed `protocol_name`s) exists to keep passing.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest,
    GetResourceIdsRequest, GetResourceStatusRequest, ModuleAndResourceId, ModuleHandle, PinData,
    ResourceData, create_com_logical_link_request, module_and_resource_id, resource_data,
};

use crate::harness::*;

// ── D-PDU `CP_ChangeSpeed*`/`CP_SwCan_HighVoltage` ComParam IDs
// (`PDU_PC_COM` class, Unum32, CAN family) -- specific to this file, not
// shared via `harness.rs`, matching `comparam_tx.rs`'s own local
// `CP_SCI_TRANSMIT_MODE`/`CP_SCI_SET_PROG_VOLTAGE` convention for a
// one-file-only ComParam ID. ──────────────────────────────────────────────

/// D-PDU `CP_ChangeSpeedCtrl`: translates to `CONFIG_SW_CAN_SPEEDCHANGE_ENABLE`
/// on an SW link (ADR-164 Decision 2).
const CP_CHANGE_SPEED_CTRL: u32 = 0x8030;

/// D-PDU `CP_ChangeSpeedMessage`: accepted-but-unmapped, no documented native
/// target (ADR-164 Decision 2).
const CP_CHANGE_SPEED_MSG: u32 = 0x8031;

/// D-PDU `CP_ChangeSpeedRate`: translates to `CONFIG_SW_CAN_HS_DATA_RATE` on
/// an SW link (ADR-164 Decision 2).
const CP_CHANGE_SPEED_RATE: u32 = 0x8032;

/// D-PDU `CP_ChangeSpeedResCtrl`: translates to `CONFIG_SW_CAN_RES_SWITCH` on
/// an SW link (ADR-164 Decision 2).
const CP_CHANGE_SPEED_RES_CTRL: u32 = 0x8033;

/// D-PDU `CP_ChangeSpeedTxDelay`: accepted-but-unmapped, no documented native
/// target (ADR-164 Decision 2).
const CP_CHANGE_SPEED_TX_DELAY: u32 = 0x8019;

/// D-PDU `CP_SwCan_HighVoltage`: nonzero ORs `TX_FLAG_SW_CAN_HV_TX` into a
/// send's TxFlags, gated on the link's SW hw id (ADR-164 Decision 2/
/// Consequences).
const CP_SW_CAN_HIGH_VOLTAGE: u32 = 0x8037;

/// Resource id of the SWCAN sibling of the native `ISO_15765_2` resource
/// (0x0206) -- `resources.rs` row 0x022B, `hw_protocol_override =
/// PROTOCOL_SW_ISO15765_PS`. The primary link used by most tests in this
/// file: ISO15765-family addressing (`set_can_phys_req_id_and_promote`) is
/// already exercised elsewhere and lets `CP_ChangeSpeed*`/`CP_SwCan_
/// HighVoltage` round-trip exactly like any other CAN-family ComParam.
const SW_ISO15765_RESOURCE_ID: u32 = 0x022B;

/// Resource id of the SWCAN sibling of the native `ISO_11898_RAW` resource
/// (0x0201) -- `resources.rs` row 0x0226, `hw_protocol_override =
/// PROTOCOL_SW_CAN_PS`.
const SW_CAN_RAW_RESOURCE_ID: u32 = 0x0226;

/// A representative SAE J2411 SWCAN normal-speed baud rate (GMLAN); the mock
/// does not validate baud rate values, so any nonzero value would work, but
/// a domain-plausible one keeps these tests self-documenting.
const SWCAN_BAUD_RATE: u32 = 33_300;

/// FTCAN sibling of the native `ISO_15765_2` resource (0x0206) --
/// `resources.rs` row 0x0235, `hw_protocol_override = PROTOCOL_FT_ISO15765_PS`
/// (same row `ft_can.rs`'s own `FT_ISO15765_RESOURCE_ID` names). Used only by
/// this file's ADR-172 regression test proving bit 17's Fault-Tolerant CAN
/// meaning (`LINK_FAULT`, clause 20 Table 86) is untouched by the new
/// SW-CAN-only withhold gate -- not duplicated from `ft_can.rs` since that
/// module's own helpers are private to it.
const FT_ISO15765_RESOURCE_ID: u32 = 0x0235;

/// SAE J2534-2 clause 9.4.1.1 Table 12 `RxStatus` bit 17 (`0x00020000`) --
/// `RX_FLAG_SW_CAN_HS_RX` in `j2534_v0404.h`. See `RX_SW_CAN_HS_RX`'s own doc
/// comment in `events.rs` (ADR-172) for the full rationale; duplicated here
/// (not imported) since `events.rs`'s constant is private to the service
/// crate.
const RX_STATUS_SW_CAN_HS_RX: u32 = 0x0002_0000;

/// SAE J2534-2 clause 9.4.1.1 Table 12 `RxStatus` bit 18 (`0x00040000`) --
/// `RX_FLAG_SW_CAN_NS_RX` in `j2534_v0404.h`. See `RX_STATUS_SW_CAN_HS_RX`'s
/// doc comment above (ADR-172) -- same rationale.
const RX_STATUS_SW_CAN_NS_RX: u32 = 0x0004_0000;

/// SAE J2534-2 clause 9.4.1.1 Table 12 `RxStatus` bit 16 (`0x00010000`) --
/// `RX_FLAG_SW_CAN_HV_RX` in `j2534_v0404.h`, genuine SW-CAN content (ADR-172
/// Decision 3): NOT withheld by the new gate, unlike bits 17/18 above. Used
/// by this file's regression test proving the gate doesn't over-exclude.
const RX_STATUS_SW_CAN_HV_RX: u32 = 0x0001_0000;

/// J2534 v04.04 `RxStatus` bit 0, `RX_TX_MSG_TYPE` (a CONFIG_LOOPBACK echo
/// indication, ADR-098) -- combined with [`RX_STATUS_SW_CAN_HS_RX`] by this
/// file's ADR-172 test proving the new withhold check runs independently of/
/// before `RX_STATUS_FLAGS_MASK`'s own low-bit gate.
const RX_STATUS_TX_MSG_TYPE: u32 = 0x0000_0001;

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)` -- same per-file local helper shape as
/// `pdu_ioctl.rs`'s own (ADR-164's originating brief; not shared via
/// `harness.rs`, matching this codebase's existing convention).
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

/// Issues `IoCtl` against a `cll_handle`, no input/output data -- same shape
/// as `pdu_ioctl.rs`'s own `io_ctl_cll`.
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

/// Creates (but does not connect) a CLL for `resource_id`, on module 1 --
/// same shape as `additional_channels.rs`'s/`fd_can.rs`'s per-file
/// `try_create_cll_with_resource`, specialized to the numeric-resource-id
/// `ProtocolId` route every existing resource-id-based test in
/// `resources.rs` already uses (e.g. `create_com_logical_link_resolves_sci_
/// config_resource_id`'s `0x0225`) -- a `ResourceData.protocol_id` value
/// inside the reserved resource-id range resolves as a direct resource-id
/// lookup, not a raw hardware `ProtocolID`.
async fn create_cll_for_resource_id(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource_id: u32,
    _cll_tag: u64,
) -> ComLogicalLinkHandle {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(resource_id),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

/// Like [`create_cll_for_resource_id`], but also stages `params` and connects
/// -- same shape as `harness::create_and_connect_cll`, specialized to a
/// resource id.
async fn create_and_connect_cll_for_resource_id(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource_id: u32,
    params: &[(u32, u32)],
) -> ComLogicalLinkHandle {
    let cll_handle = create_cll_for_resource_id(client, resource_id, 1).await;

    for &(com_param_id, value) in params {
        set_com_param_unum32(client, cll_handle, com_param_id, value).await;
    }

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    cll_handle
}

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// every SW resource row requires this opt-in (`names.rs`'s SW arm in
/// `resolve_pin_selection`, mirroring the FD-family opt-in gate).
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

/// Item 1: connecting via an SW resource id succeeds and the connect
/// sequence includes the internal `SET_CONFIG(CONFIG_J1962_PINS)` call --
/// clause 9.2.1's model of no implicit default pin, with an explicit pin
/// assignment always required (ADR-164 Decision 1), unlike a base-id'd `_PS` family's own
/// optional-pins default connect.
#[tokio::test]
#[serial]
async fn connecting_an_sw_resource_emits_the_internal_pins_set_config() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_ISO15765_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, SWCAN_BAUD_RATE)],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_SW_ISO15765_PS,
        "connecting resource 0x022B should open a native SW_ISO15765_PS channel"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0100,
        "an unqualified SW connect (no dlc_pin_data) should assign the row's own default pin \
         (pin 1, PIN_HI), packed 0x0000PPSS with SS = 0 (no secondary pin)"
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "CONFIG_J1962_PINS should have been SET_CONFIG'd during connect; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Item 2: `SetComParam`/`GetComParam` round-trips all 5 `CP_ChangeSpeed*`
/// ComParams on an SW link -- ADR-017's pre-Phase-4 rejection is reversed
/// (ADR-164 Decision 2). The 3 with a documented native mapping
/// (`Rate`/`Ctrl`/`ResCtrl`) actually reach the mock's stored `SET_CONFIG`
/// value.
#[tokio::test]
#[serial]
async fn change_speed_comparams_round_trip_and_the_mapped_three_reach_native_config() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_ISO15765_RESOURCE_ID,
        &[
            (j2534_0404::DATA_RATE, SWCAN_BAUD_RATE),
            (CP_CHANGE_SPEED_RATE, 83_300),
            (CP_CHANGE_SPEED_CTRL, 1),
            (CP_CHANGE_SPEED_RES_CTRL, 1),
            (CP_CHANGE_SPEED_TX_DELAY, 50),
        ],
    )
    .await;
    // CP_ChangeSpeedMessage is a Bytefield param (unlike the other 4, all
    // Unum32) -- set separately via the Bytefield oneof variant.
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_CHANGE_SPEED_MSG,
        vec![0xA5, 0x02],
    )
    .await;

    // Round-trip: GetComParam echoes back exactly what was staged (was
    // rejected outright pre-Phase-4).
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_CHANGE_SPEED_RATE).await,
        83_300
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_CHANGE_SPEED_CTRL).await,
        1
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_CHANGE_SPEED_RES_CTRL).await,
        1
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_CHANGE_SPEED_TX_DELAY).await,
        50
    );
    let msg_response = client
        .get_com_param(vci_service_interface::GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(
                vci_service_interface::get_com_param_request::Param::ParamId(CP_CHANGE_SPEED_MSG),
            ),
        })
        .await
        .expect("get_com_param(CP_ChangeSpeedMessage) should succeed")
        .into_inner();
    assert_eq!(
        msg_response.param_item.and_then(|p| p.param_data),
        Some(vci_service_interface::param_item::ParamData::Bytefield(
            vec![0xA5, 0x02]
        )),
        "CP_ChangeSpeedMessage should round-trip through GetComParam"
    );

    // The 3 documented translations (ISO 22900-2:2009 Annex A.1.2 Table A.3)
    // actually reached the mock's native SET_CONFIG storage.
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_SW_CAN_HS_DATA_RATE),
        83_300,
        "CP_ChangeSpeedRate should forward to CONFIG_SW_CAN_HS_DATA_RATE"
    );
    assert_eq!(
        server.backdoor.config_value(
            MOCK_CHANNEL_ID,
            j2534_0404::CONFIG_SW_CAN_SPEEDCHANGE_ENABLE
        ),
        1,
        "CP_ChangeSpeedCtrl should forward to CONFIG_SW_CAN_SPEEDCHANGE_ENABLE"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_SW_CAN_RES_SWITCH),
        1,
        "CP_ChangeSpeedResCtrl should forward to CONFIG_SW_CAN_RES_SWITCH"
    );

    server.shutdown().await;
}

/// Item 3: `CP_ChangeSpeedMsg`/`CP_ChangeSpeedTxDelay` accept via
/// `SetComParam`/`GetComParam` (confirmed by the round-trip test above) but
/// do NOT reach any native `SET_CONFIG` call -- `to_j2534_config_id` returns
/// `None` for both (ISO 22900-2:2009 Annex A.1.2 Table A.3 documents no
/// native mapping for either), so `apply_j2534_params`'s `filter_map` drops
/// them before ever building the native `SET_CONFIG` batch.
#[tokio::test]
#[serial]
async fn change_speed_msg_and_tx_delay_never_reach_native_set_config() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll_for_resource_id(&mut client, SW_ISO15765_RESOURCE_ID, 1).await;
    set_com_param_unum32(
        &mut client,
        cll_handle,
        j2534_0404::DATA_RATE,
        SWCAN_BAUD_RATE,
    )
    .await;
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_CHANGE_SPEED_MSG,
        vec![0xA5, 0x02],
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_CHANGE_SPEED_TX_DELAY, 50).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        !log.contains(&j2534_0404::CONFIG_SW_CAN_HS_DATA_RATE)
            && !log.contains(&j2534_0404::CONFIG_SW_CAN_SPEEDCHANGE_ENABLE)
            && !log.contains(&j2534_0404::CONFIG_SW_CAN_RES_SWITCH),
        "neither CP_ChangeSpeedMsg nor CP_ChangeSpeedTxDelay has a native SET_CONFIG target, so \
         none of the 3 mapped CONFIG_SW_CAN_* ids should appear in the log; log was {log:#x?}"
    );
    assert_eq!(
        log,
        vec![j2534_0404::CONFIG_J1962_PINS],
        "only the mandatory pin-assignment SET_CONFIG should have reached the mock"
    );

    server.shutdown().await;
}

/// Item 4: `CP_SwCan_HighVoltage` set nonzero causes a subsequent
/// `CoptSendrecv` on the SW link to carry `TX_FLAG_SW_CAN_HV_TX` -- mirrors
/// `comparam_tx.rs`'s `sci_tx_flags_derived_from_transmit_mode_and_prog_
/// voltage_comparams` (ADR-062 precedent), the same "ComParam -> per-message
/// TxFlags bit" shape cloned for SWCAN (ADR-164 Decision 2/Consequences).
#[tokio::test]
#[serial]
async fn sw_can_high_voltage_comparam_sets_tx_flag_on_an_sw_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_ISO15765_RESOURCE_ID,
        &[
            (j2534_0404::DATA_RATE, SWCAN_BAUD_RATE),
            (CP_SW_CAN_HIGH_VOLTAGE, 1),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    send_data(&mut client, cll_handle, vec![0x01, 0x02], vec![]).await;
    assert_ne!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0) & j2534_0404::TX_FLAG_SW_CAN_HV_TX,
        0,
        "a nonzero CP_SwCan_HighVoltage should set TX_FLAG_SW_CAN_HV_TX on an SW-connected link's \
         send"
    );

    server.shutdown().await;
}

/// Item 5 (regression for the SW-hw-id gate `apply_resolved_tx_flags`
/// added): the same `CP_SwCan_HighVoltage` set nonzero on a plain dual-wire
/// ISO15765 link (not SW) must NOT bleed the flag through -- `PARAM_SW_CAN_
/// HIGH_VOLTAGE` is allowlisted CAN-family-wide (Decision 2), so a dual-wire
/// link's ComParams can legitimately carry a nonzero value here even though
/// it must never reach the wire.
#[tokio::test]
#[serial]
async fn sw_can_high_voltage_comparam_does_not_bleed_onto_a_dual_wire_can_link() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::ISO15765,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_SW_CAN_HIGH_VOLTAGE, 1),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::ISO15765,
        "sanity: this link must actually be a plain dual-wire ISO15765 link, not SW-substituted"
    );
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    send_data(&mut client, cll_handle, vec![0x01, 0x02], vec![]).await;
    assert_eq!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0) & j2534_0404::TX_FLAG_SW_CAN_HV_TX,
        0,
        "CP_SwCan_HighVoltage must never reach TxFlags on a non-SW (plain dual-wire) CAN-family \
         link, even though the ComParam is allowlisted family-wide"
    );

    server.shutdown().await;
}

/// Item 6: `PDU_IOCTL_SW_CAN_HS`/`PDU_IOCTL_SW_CAN_NS` succeed on an
/// SW-connected CLL, and are rejected on a non-SW CLL with the exact
/// `PDU_ERR_ID_NOT_SUPPORTED`/`Unimplemented` mapping `rpc_misc::
/// ioctl_sw_can_mode` actually produces (ADR-164 Decision 3).
#[tokio::test]
#[serial]
async fn sw_can_hs_ns_ioctls_succeed_on_sw_link_and_reject_on_non_sw_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let sw_cll = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_CAN_RAW_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, SWCAN_BAUD_RATE)],
    )
    .await;
    let can_cll = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let hs_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SW_CAN_HS").await;
    let ns_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SW_CAN_NS").await;

    io_ctl_cll(&mut client, sw_cll, hs_id)
        .await
        .expect("PDU_IOCTL_SW_CAN_HS should succeed on an SW-connected CLL");
    io_ctl_cll(&mut client, sw_cll, ns_id)
        .await
        .expect("PDU_IOCTL_SW_CAN_NS should succeed on an SW-connected CLL");
    assert_eq!(
        server.backdoor.sw_can_hs_count(),
        1,
        "the native SW_CAN_HS IOCTL should have reached the mock exactly once"
    );
    assert_eq!(
        server.backdoor.sw_can_ns_count(),
        1,
        "the native SW_CAN_NS IOCTL should have reached the mock exactly once"
    );

    let hs_status = io_ctl_cll(&mut client, can_cll, hs_id)
        .await
        .expect_err("PDU_IOCTL_SW_CAN_HS should be rejected on a non-SW CLL");
    assert_eq!(hs_status.code(), Code::Unimplemented);
    assert!(hs_status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"));
    let ns_status = io_ctl_cll(&mut client, can_cll, ns_id)
        .await
        .expect_err("PDU_IOCTL_SW_CAN_NS should be rejected on a non-SW CLL");
    assert_eq!(ns_status.code(), Code::Unimplemented);
    assert!(ns_status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"));
    assert_eq!(
        server.backdoor.sw_can_hs_count(),
        1,
        "the rejected non-SW attempt must not have reached the native IOCTL"
    );
    assert_eq!(
        server.backdoor.sw_can_ns_count(),
        1,
        "the rejected non-SW attempt must not have reached the native IOCTL"
    );

    server.shutdown().await;
}

/// Item 7: `PDU_IOCTL_SW_CAN_HS` on a channel shared by two CLLs
/// (`SharedChannel::ref_count > 1`) is a no-op (`Ok`, no native call) rather
/// than an error -- ADR-164 Consequences, mirroring `PDU_IOCTL_CLEAR_TX_QUEUE`'s
/// existing `ref_count > 1` skip precedent. Two CLLs created against the same
/// SW resource id with identical params share one physical channel (same
/// technique `pdu_ioctl.rs`'s `clear_tx_queue_checks_physical_lock_before_
/// cancelling_held_items` uses for a plain CAN channel).
#[tokio::test]
#[serial]
async fn sw_can_hs_is_a_no_op_on_a_shared_physical_channel() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_ISO15765_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, SWCAN_BAUD_RATE)],
    )
    .await;
    let cll_b = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_ISO15765_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, SWCAN_BAUD_RATE)],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "sanity: cll_a and cll_b must actually share one physical channel for this test to \
         exercise the ref_count > 1 gate at all"
    );

    let hs_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SW_CAN_HS").await;
    io_ctl_cll(&mut client, cll_a, hs_id)
        .await
        .expect("PDU_IOCTL_SW_CAN_HS on a shared channel should be a no-op success, not an error");
    assert_eq!(
        server.backdoor.sw_can_hs_count(),
        0,
        "no native SW_CAN_HS IOCTL should have reached the mock while the channel is shared \
         (ref_count == 2)"
    );

    let _ = cll_b;
    server.shutdown().await;
}

/// Item 8: staging FD ComParams (`CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate`)
/// on an SW-connected link and then connecting is rejected outright, not
/// silently substituted to `FD_ISO15765_PS` -- regression test for the guard
/// `rpc_link::J2534Service::apply_fd_mode` added (ADR-164 Decision 1).
#[tokio::test]
#[serial]
async fn staging_fd_comparams_on_an_sw_link_is_rejected_not_substituted() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll_for_resource_id(&mut client, SW_ISO15765_RESOURCE_ID, 1).await;
    set_com_param_unum32(
        &mut client,
        cll_handle,
        j2534_0404::DATA_RATE,
        SWCAN_BAUD_RATE,
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_BAUDRATE, 2_000_000).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "connecting an SW_ISO15765_PS-resolved CLL with FD ComParams staged must be \
             rejected, not silently substituted to FD_ISO15765_PS",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("Single Wire CAN"),
        "the rejection should name the SWCAN-vs-FD conflict: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejection must happen before any native PassThruConnect is attempted"
    );

    server.shutdown().await;
}

/// Item 9 (Bug 1 fix, edge-case-hunter audit): connecting only the SWCAN
/// resource (0x0226) must NOT make `GetResourceStatus` report its plain
/// dual-wire CAN sibling (0x0201, `hw_protocol_override: None`) as "in use"
/// (bit 0x01) -- the two are physically distinct buses (different pins,
/// different `bus_type_id`, ADR-164 Context/Consequences), even though the
/// SW link's `base_hw_protocol_id()` normalizes to the same `CAN` identity
/// the dual-wire row's own `status_hw_id` uses (ADR-164 Decision 1's Plane B
/// funnel). Before the fix, `matches_status_hw_id` OR'd that normalized
/// `base` in as a match candidate unconditionally, so this query falsely
/// reported the dual-wire resource occupied. This is a permanent
/// reinstatement of a repro an edge-case-hunter audit found and reverted as
/// a temporary test -- see this file's originating brief.
#[tokio::test]
#[serial]
async fn get_resource_status_does_not_let_a_connected_sw_can_link_occupy_its_dual_wire_sibling() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _sw_cll = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_CAN_RAW_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, SWCAN_BAUD_RATE)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(0x0201)), // ISO_11898_RAW (plain dual-wire CAN)
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();

    let resource_status_data = response
        .resource_status
        .expect("response should carry a ResourceStatusItem")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_status & 0x01,
        0,
        "the plain dual-wire CAN resource (0x0201) must NOT report bit 0 (in use) while only its \
         SWCAN sibling (0x0226) is connected -- they are physically distinct buses (ADR-164)"
    );

    server.shutdown().await;
}

/// Item 10 (Bug 1 fix, reverse direction): the correct existing behavior this
/// diff's own ADR-164 correction already established -- connecting only the
/// plain dual-wire CAN resource (0x0201) must NOT make `GetResourceStatus`
/// report the SWCAN resource (0x0226) as "in use" either. Kept alongside
/// Item 9 so both directions of the SW-vs-dual-wire physical-resource
/// distinction are pinned by regression tests in one place.
#[tokio::test]
#[serial]
async fn get_resource_status_does_not_let_a_connected_dual_wire_can_link_occupy_its_sw_can_sibling()
{
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _can_cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    SW_CAN_RAW_RESOURCE_ID,
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();

    let resource_status_data = response
        .resource_status
        .expect("response should carry a ResourceStatusItem")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_status & 0x01,
        0,
        "the SWCAN resource (0x0226) must NOT report bit 0 (in use) while only its plain \
         dual-wire CAN sibling (0x0201) is connected -- they are physically distinct buses \
         (ADR-164)"
    );

    server.shutdown().await;
}

/// Item 11 (Codex review round 2 finding on the FTCAN sibling of this same
/// mechanism, `ft_can.rs`, P2, fixed here too since it's the identical
/// pre-existing bug in this file's own `resolve_pin_selection` SW arm):
/// naming the raw `SW_ISO15765_PS` hardware protocol id directly (bypassing
/// the resource table entirely -- `resolve_protocol_id` finds no matching
/// row for this numeric value, so `resource_row` is `None`) with no
/// `dlc_pin_data` must be rejected, not silently connected with the
/// resource-table row's own single-pin default (`0x0000_0100`) -- that
/// default only reflects a choice this file's *table row* made, not a
/// choice the caller made; clause 9.2.1 itself has no default pin at all.
#[tokio::test]
#[serial]
async fn connecting_the_raw_sw_protocol_id_directly_without_pins_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_SW_ISO15765_PS,
                    )),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "naming the raw SW_ISO15765_PS id directly with no dlc_pin_data must be rejected, \
             not silently defaulted to the resource-table row's own pin",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("no default pin"),
        "the rejection should explain that clause 9.2.1 has no default pin: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Codex review finding, PR #57 (found while adding the sibling UART Echo
/// Byte arm, which copied this exact same pre-existing shape -- fixed in
/// both arms in the same PR): `dlc_pin_data` supplying a SECOND pin
/// alongside a valid primary must be rejected, not silently packed via the
/// general clause-6 two-pin path into `CONFIG_J1962_PINS` -- clause 9's
/// SW bus is single-wire and its resource row defines exactly one pin, with
/// no secondary role to pair a second entry with.
#[tokio::test]
#[serial]
async fn connecting_the_raw_sw_protocol_id_with_two_pins_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![
                        PinData {
                            dlc_pin_number: 1,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "HI".to_string(),
                                ),
                            ),
                        },
                        PinData {
                            dlc_pin_number: 9,
                            dlc_pin_type: Some(
                                vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                    "LOW".to_string(),
                                ),
                            ),
                        },
                    ],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_SW_ISO15765_PS,
                    )),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "supplying a secondary pin for the single-wire SW_ISO15765_PS bus must be rejected, \
             not silently packed as a two-pin CONFIG_J1962_PINS value",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("single-wire"),
        "the rejection should explain this bus has no secondary pin role: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Sibling gap to the FT-CAN closed-set pin check (ADR-168's "Seventh
/// correction") that this arm previously lacked: clause 9.2.1 attaches
/// SW_CAN to J1962 pin 1 only, a closed set of exactly one pin, but a
/// single well-formed pin OTHER than pin 1 (here, pin 6/HI) was previously
/// accepted and forwarded to `SET_CONFIG(J1962_PINS)` unchecked -- a
/// physically meaningless value for this single-wire bus. Must be
/// rejected, not silently accepted just because `compute_pin_select`
/// packs it into a valid-looking bitmask.
#[tokio::test]
#[serial]
async fn connecting_the_raw_sw_protocol_id_with_a_wrong_pin_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![PinData {
                        dlc_pin_number: 6,
                        dlc_pin_type: Some(
                            vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                "HI".to_string(),
                            ),
                        ),
                    }],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_SW_ISO15765_PS,
                    )),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "pin 6/HI is not the one pin clause 9.2.1 defines for SW_CAN and must be rejected, \
             not silently accepted just because it packs to a well-formed bitmask",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("9.2.1"),
        "the rejection should cite clause 9.2.1's one defined pin: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Item 12 (Codex review round 3 finding on the FTCAN sibling of this same
/// mechanism, `ft_can.rs`, P2, fixed here too as the identical pre-existing
/// bug in this file's own query-side resolution): querying
/// `GetResourceStatus` with the raw `SW_ISO15765_PS` hardware protocol id
/// directly as a `ResourceId` (bypassing the resource table entirely) must
/// correctly report bit 0 ("in use") when an SW link is actually
/// connected, not silently normalize the query to plain ISO15765 and miss
/// it.
#[tokio::test]
#[serial]
async fn get_resource_status_with_the_raw_sw_protocol_id_finds_a_connected_sw_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _sw_cll = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_ISO15765_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, SWCAN_BAUD_RATE)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    j2534_0404::PROTOCOL_SW_ISO15765_PS,
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();

    let resource_status_data = response
        .resource_status
        .expect("response should carry a ResourceStatusItem")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_status & 0x01,
        1,
        "querying the raw SW_ISO15765_PS id directly must report bit 0 (in use) when an SW \
         link is actually connected, not silently normalize the query to plain ISO15765 and \
         miss it"
    );

    server.shutdown().await;
}

/// Item 12 (reverse direction): a plain dual-wire ISO15765 link must NOT
/// satisfy a raw `SW_ISO15765_PS` `ResourceId` query.
#[tokio::test]
#[serial]
async fn get_resource_status_with_the_raw_sw_protocol_id_does_not_match_a_plain_dual_wire_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _can_cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        j2534_0404::ISO15765,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    j2534_0404::PROTOCOL_SW_ISO15765_PS,
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();

    let resource_status_data = response
        .resource_status
        .expect("response should carry a ResourceStatusItem")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_status & 0x01,
        0,
        "querying the raw SW_ISO15765_PS id directly must NOT report bit 0 (in use) when only \
         a plain dual-wire ISO15765 link is connected -- they are physically distinct buses"
    );

    server.shutdown().await;
}

/// Item 13 (edge-case-hunter finding, coverage gap): Items 11 and 12 above
/// are each tested in isolation -- Item 11 only exercises the *rejected*
/// no-pins half of naming the raw `SW_ISO15765_PS` id directly, and Item 12
/// connects via the resource-*table* id (`SW_ISO15765_RESOURCE_ID`) rather
/// than the raw bypass route before querying by raw id. This test
/// exercises both fixes together: connecting via the raw `SW_ISO15765_PS`
/// hardware protocol id directly (bypassing the resource table entirely,
/// `resource_row: None`) WITH the one pin clause 9.2.1 defines (pin 1/HI)
/// succeeds (the accepted case of Item 11 -- only the no-pins case, and
/// since the SWCAN pin allow-list fix, any OTHER explicit pin, are
/// rejected), and the resulting connected link is then correctly found by
/// a `GetResourceStatus` query keyed on that same raw id (Item 12's
/// query-side fix).
#[tokio::test]
#[serial]
async fn connecting_the_raw_sw_protocol_id_with_explicit_pins_then_querying_by_raw_id_finds_the_link()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![PinData {
                        dlc_pin_number: 1,
                        dlc_pin_type: Some(
                            vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                "HI".to_string(),
                            ),
                        ),
                    }],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_SW_ISO15765_PS,
                    )),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

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
        .expect(
            "connecting the raw SW_ISO15765_PS id directly with explicit dlc_pin_data selecting \
             pin 1/HI should succeed -- only the no-pins case, and any other pin, are rejected",
        );
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_SW_ISO15765_PS,
        "connecting the raw SW_ISO15765_PS id directly with explicit pins should still open a \
         native SW_ISO15765_PS channel"
    );

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    j2534_0404::PROTOCOL_SW_ISO15765_PS,
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();

    let resource_status_data = response
        .resource_status
        .expect("response should carry a ResourceStatusItem")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_status & 0x01,
        1,
        "a link connected via the raw SW_ISO15765_PS id with explicit dlc_pin_data (no \
         resource-table row match) must still report bit 0 (in use) when queried by that same \
         raw id, not be silently missed"
    );

    server.shutdown().await;
}

/// Item 14 (this PR's brief, generalizing the Honda DIAG-H/SAE J1708
/// `rpc_create_com_logical_link` fallback arms to SWCAN via
/// `resources::bustype_default_name_for_hw_protocol_id`): naming the raw
/// `SW_CAN_PS` hardware protocol id directly (bypassing the resource table
/// entirely, `resource_row == None`) with no caller-supplied `bus_type_name`
/// and no `SetComParam(DATA_RATE, ...)` staged must still receive Table
/// B.21's own 33_333bps SAE J2411 SWCAN default -- before this fix, the
/// Working ComParamSet stayed empty on this route and `PassThruConnect`
/// received a native baud rate of 0.
#[tokio::test]
#[serial]
async fn connecting_the_raw_sw_protocol_id_seeds_the_33333_baud_rate_default() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![PinData {
                        dlc_pin_number: 1,
                        dlc_pin_type: Some(
                            vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                "HI".to_string(),
                            ),
                        ),
                    }],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_SW_CAN_PS,
                    )),
                },
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

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.baud_rate(MOCK_CHANNEL_ID),
        33_333,
        "the raw-id no-resource-row SW connect route must still receive Table B.21's own \
         33_333bps SAE_J2411_SWCAN default, not the empty Working set's default of 0"
    );

    server.shutdown().await;
}

/// Item 15 (sibling of Item 14, edge-case-hunter-style regression): the same
/// raw `SW_CAN_PS` connect, but WITH a caller-supplied, unrelated,
/// well-formed `bus_type_name` (`RscData.bus_type`/`.protocol` are
/// independent fields with no cross-validation) -- this SW link's own fixed
/// bustype identity must win, not the mismatched name's own bustype
/// defaults.
#[tokio::test]
#[serial]
async fn connecting_the_raw_sw_protocol_id_ignores_a_mismatched_bus_type_name() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![PinData {
                        dlc_pin_number: 1,
                        dlc_pin_type: Some(
                            vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                "HI".to_string(),
                            ),
                        ),
                    }],
                    bus_type: Some(resource_data::BusType::BusTypeName(
                        "ISO_14230_1_UART".to_string(),
                    )),
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_SW_CAN_PS,
                    )),
                },
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

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.baud_rate(MOCK_CHANNEL_ID),
        33_333,
        "a mismatched bus_type_name must not override this SW link's own SAE_J2411_SWCAN \
         33_333bps default"
    );

    server.shutdown().await;
}

/// Item 16 (ISO15765 sub-variant landmine, this PR's design-consult
/// finding): `PROTOCOL_SW_ISO15765_PS`'s own `ChannelProtocol` normalizes to
/// plain `ISO15765` (the generic CAN-family identity,
/// `resources::base_protocol_id`), NOT the SW family -- so a fix keyed on
/// `protocol.j2534_protocol_id()` alone would silently never recognize this
/// sub-variant. `SW_CAN_PS` (Item 14 above) is equally affected by the same
/// landmine -- its own `base_protocol_id` normalizes to plain `CAN`, not its
/// own raw id either -- so this dedicated ISO15765-variant test exists to
/// pin the identical landmine for the ISO15765 framing specifically, proving
/// the fix is keyed on `pin_selection`'s own `ps_protocol_id`, not the
/// normalized `protocol`, for both variants.
#[tokio::test]
#[serial]
async fn connecting_the_raw_sw_iso15765_protocol_id_seeds_the_33333_baud_rate_default() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![PinData {
                        dlc_pin_number: 1,
                        dlc_pin_type: Some(
                            vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                "HI".to_string(),
                            ),
                        ),
                    }],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_SW_ISO15765_PS,
                    )),
                },
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

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.baud_rate(MOCK_CHANNEL_ID),
        33_333,
        "the raw-id no-resource-row SW_ISO15765_PS connect route must still receive the \
         33_333bps SAE_J2411_SWCAN default, proving the fix keys on pin_selection's own \
         ps_protocol_id, not the normalized-to-base protocol identity"
    );

    server.shutdown().await;
}

/// Issues `GetResourceIds` filtered on `resource_data::Protocol::ProtocolId(id)`
/// and returns the resolved resource IDs in table order -- same shape as
/// `resources.rs`'s own `get_resource_ids` helper, specialized to this file's
/// module handle constant.
async fn get_resource_ids_by_protocol_id(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    protocol_id: u32,
) -> Vec<u32> {
    client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource_data: Some(ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(resource_data::Protocol::ProtocolId(protocol_id)),
            }),
        })
        .await
        .expect("get_resource_ids should succeed")
        .into_inner()
        .resource_id_list
        .and_then(|list| list.resource_id_data_array.into_iter().next())
        .map(|data| data.resource_id_array)
        .unwrap_or_default()
}

/// Codex review finding, mirrored from `ft_can.rs`'s identical FT regression:
/// `candidates_matching_protocol`'s `ProtocolId` fallback never consulted
/// `row.hw_protocol_override`, so a raw `_PS` hardware protocol id -- the
/// only place the SW `_PS` ids live for every SW resource row, `resources.rs`
/// rows 0x0226-0x022F -- matched zero table rows even though it can be used
/// directly with `CreateComLogicalLink`/`ConnectComLogicalLink`/
/// `GetResourceStatus`, as this file's other tests show:
/// `GetResourceIds(protocol_id = PROTOCOL_SW_CAN_PS)` must resolve the
/// raw-CAN SWCAN row (0x0226), and `GetResourceIds(protocol_id =
/// PROTOCOL_SW_ISO15765_PS)` must resolve every ISO15765-based SWCAN row
/// (0x0227-0x022F).
#[tokio::test]
#[serial]
async fn get_resource_ids_by_raw_sw_protocol_id_resolves_the_sw_rows() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let sw_can_ids =
        get_resource_ids_by_protocol_id(&mut client, j2534_0404::PROTOCOL_SW_CAN_PS).await;
    assert_eq!(
        sw_can_ids,
        vec![SW_CAN_RAW_RESOURCE_ID],
        "PROTOCOL_SW_CAN_PS should resolve only the raw-CAN SWCAN row (0x0226), via \
         hw_protocol_override, not the empty list the pre-fix fallback returned"
    );

    let sw_iso15765_ids =
        get_resource_ids_by_protocol_id(&mut client, j2534_0404::PROTOCOL_SW_ISO15765_PS).await;
    assert_eq!(
        sw_iso15765_ids,
        vec![
            0x0227, 0x0228, 0x0229, 0x022A, 0x022B, 0x022C, 0x022D, 0x022E, 0x022F
        ],
        "PROTOCOL_SW_ISO15765_PS should resolve every ISO15765-based SWCAN row, via \
         hw_protocol_override, not the empty list the pre-fix fallback returned"
    );

    server.shutdown().await;
}

// ── ADR-172: SW-CAN speed-transition confirmation frame withhold ──────────
//
// SAE J2534-2 clause 9.4.1.1 Table 12 RxStatus bits 17/18 (SW_CAN_HS_RX/
// SW_CAN_NS_RX) carry explicitly undefined data content and must be withheld
// entirely from a SW-CAN-family link (not merely excluded from content-
// eligible matching), while bit 16 (SW_CAN_HV_RX, genuine SW-CAN content) and
// bit 17's completely different Fault-Tolerant CAN meaning (LINK_FAULT) stay
// untouched. See `events.rs`'s `RX_SW_CAN_HS_RX`/`RX_SW_CAN_NS_RX` doc
// comments and ADR-172 itself for the full rationale.
//
// Every test below connects with functional addressing (`CP_RequestAddrMode`
// = 2 / `CP_CanFuncReqId`) instead of a `UniqueRespIdTable` entry -- mirrors
// `rx_header_split.rs`'s `can_protocol_splits_can_id_header_into_extra_info`.
// A table entry with only `CP_CanPhysReqId` set (no `CP_CanRespUsdtId`) makes
// `entry.unique_resp_ids` non-empty with an unmatchable (`usdt`/`uudt` both
// `None`) row, which makes `route_frame` drop every injected frame BEFORE it
// ever reaches `bind_frame`/the Tier-2 `arm_receive_only_monitor` registrant
// -- silently making a delivery assertion vacuous. Functional addressing
// resolves `arm_receive_only_monitor`'s own dummy (never-sent) COP without
// touching `UniqueRespIdTable` at all, keeping it empty and routing every
// frame through unconditionally (`route_frame`'s "no-table mode").

/// Item (a): a SW-CAN-family (raw-CAN `SW_CAN_PS`) frame whose RxStatus
/// carries only bit 17 (`SW_CAN_HS_RX`) is withheld entirely -- not delivered
/// to the client at all, not merely excluded from content-eligible matching.
/// Pre-ADR-172, `RX_STATUS_FLAGS_MASK` has no bits above bit 4, so this frame
/// would evaluate `rx_status_flags == 0` and be treated as ordinary content,
/// reaching `arm_receive_only_monitor`'s broad (empty mask/pattern) matcher.
#[tokio::test]
#[serial]
async fn sw_can_speed_transition_hs_confirmation_frame_is_withheld() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_CAN_RAW_RESOURCE_ID,
        &[
            (j2534_0404::DATA_RATE, SWCAN_BAUD_RATE),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    // Data content is explicitly undefined per clause 9.4.1.1 -- any 4+ bytes
    // would do; this is deliberately what an unfixed gate would parse as a
    // plausible-looking CAN ID + payload.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0xAA, 0xBB, 0xCC, 0xDD],
        j2534_0404::PROTOCOL_SW_CAN_PS,
        RX_STATUS_SW_CAN_HS_RX,
    );

    assert_no_result_data(
        &mut client,
        cll_handle,
        "a bit-17 (SW_CAN_HS_RX) SW-CAN speed-transition confirmation frame must be withheld \
         entirely (ADR-172)",
    )
    .await;

    server.shutdown().await;
}

/// Item (b): same as item (a), for bit 18 (`SW_CAN_NS_RX`).
#[tokio::test]
#[serial]
async fn sw_can_speed_transition_ns_confirmation_frame_is_withheld() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_CAN_RAW_RESOURCE_ID,
        &[
            (j2534_0404::DATA_RATE, SWCAN_BAUD_RATE),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0xAA, 0xBB, 0xCC, 0xDD],
        j2534_0404::PROTOCOL_SW_CAN_PS,
        RX_STATUS_SW_CAN_NS_RX,
    );

    assert_no_result_data(
        &mut client,
        cll_handle,
        "a bit-18 (SW_CAN_NS_RX) SW-CAN speed-transition confirmation frame must be withheld \
         entirely (ADR-172)",
    )
    .await;

    server.shutdown().await;
}

/// Item (c) (regression guard against over-excluding): bit 16 (`SW_CAN_HV_RX`)
/// is genuine SW-CAN content (ADR-172 Decision 3) and must still be delivered
/// as ordinary content, unchanged from ADR-172 Decision 3 -- now tagged via
/// ISO 22900-2 Table D.5 `RxFlag` byte 1 bit 0 (ADR-191), rather than left
/// with an empty `rx_flag`.
#[tokio::test]
#[serial]
async fn sw_can_hv_rx_bit_is_not_withheld_and_is_delivered_as_content() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_CAN_RAW_RESOURCE_ID,
        &[
            (j2534_0404::DATA_RATE, SWCAN_BAUD_RATE),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x01, 0x02, 0x03];
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x123, &payload),
        j2534_0404::PROTOCOL_SW_CAN_PS,
        RX_STATUS_SW_CAN_HV_RX,
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(
        &result,
        &0x123_u32.to_be_bytes(),
        &[],
        &payload,
        &[0x00, 0x01, 0x00, 0x00],
    );

    server.shutdown().await;
}

/// Item (d) (the most important negative test): bit 17 means `LINK_FAULT` on
/// Fault-Tolerant CAN (clause 20, Table 86) -- a completely different,
/// still-content-eligible signal that tags a genuinely received message. A
/// naive fix checking "any CAN-family link" instead of specifically
/// `resources::is_sw_protocol_id` would wrongly withhold this too.
#[tokio::test]
#[serial]
async fn ft_can_link_fault_bit_is_not_withheld_and_is_delivered_as_content() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_resource_id(
        &mut client,
        FT_ISO15765_RESOURCE_ID,
        &[
            (j2534_0404::DATA_RATE, SWCAN_BAUD_RATE),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FT_ISO15765_PS,
        "sanity: this link must actually be FT-CAN (not SW-CAN) for this test to exercise the \
         is_sw_protocol_id scoping at all"
    );
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x62, 0xF1, 0x90];
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x7E8, &payload),
        j2534_0404::PROTOCOL_FT_ISO15765_PS,
        RX_STATUS_SW_CAN_HS_RX, // bit 17 -- LINK_FAULT on FT-CAN, not a withhold trigger
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data(&result, &0x7E8_u32.to_be_bytes(), &[], &payload);

    server.shutdown().await;
}

/// Item (e): the `SW_ISO15765_PS` variant (not just the raw-CAN `SW_CAN_PS`
/// variant items (a)-(c)/(f) use) is also covered by the withhold gate --
/// `resources::is_sw_protocol_id` matches both SW-CAN family hardware
/// protocol ids.
#[tokio::test]
#[serial]
async fn sw_iso15765_speed_transition_hs_confirmation_frame_is_also_withheld() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_ISO15765_RESOURCE_ID,
        &[
            (j2534_0404::DATA_RATE, SWCAN_BAUD_RATE),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_SW_ISO15765_PS,
        "sanity: this link must actually be the SW_ISO15765_PS variant, not SW_CAN_PS, for this \
         test to exercise anything beyond items (a)/(b)"
    );
    arm_receive_only_monitor(&mut client, cll_handle).await;

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0xAA, 0xBB, 0xCC, 0xDD],
        j2534_0404::PROTOCOL_SW_ISO15765_PS,
        RX_STATUS_SW_CAN_HS_RX,
    );

    assert_no_result_data(
        &mut client,
        cll_handle,
        "resources::is_sw_protocol_id covers PROTOCOL_SW_ISO15765_PS too -- bit 17 must be \
         withheld here as well, not just on the raw SW_CAN_PS variant (ADR-172)",
    )
    .await;

    server.shutdown().await;
}

/// Item (f): bit 17 combined with a low bit already in `RX_STATUS_FLAGS_MASK`
/// (`RX_TX_MSG_TYPE`, bit 0) is still withheld -- confirms the new check runs
/// independently of/before the existing low-bit gate, not as some kind of
/// exclusive alternative to it. Pre-ADR-172, this exact combination would
/// NOT have been withheld: `rx_status_flags` (bit 0 only, bit 17 is outside
/// `RX_STATUS_FLAGS_MASK`'s scope) makes `is_content_frame` false, so the
/// frame falls to the indication path -- and a bare `RX_TX_MSG_TYPE` bit is
/// ADR-098's CONFIG_LOOPBACK echo, which `indication_suppressed` never
/// filters, so it would still have been delivered (unattributed).
#[tokio::test]
#[serial]
async fn sw_can_speed_transition_bit_combined_with_a_low_rx_status_bit_is_still_withheld() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_CAN_RAW_RESOURCE_ID,
        &[
            (j2534_0404::DATA_RATE, SWCAN_BAUD_RATE),
            (CP_REQUEST_ADDR_MODE, 2),
            (CP_CAN_FUNC_REQ_ID, 0x7DF),
        ],
    )
    .await;
    arm_receive_only_monitor(&mut client, cll_handle).await;

    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0xAA, 0xBB, 0xCC, 0xDD],
        j2534_0404::PROTOCOL_SW_CAN_PS,
        RX_STATUS_SW_CAN_HS_RX | RX_STATUS_TX_MSG_TYPE,
    );

    assert_no_result_data(
        &mut client,
        cll_handle,
        "the ADR-172 withhold check must run independently of/before RX_STATUS_FLAGS_MASK's own \
         low-bit gate, not as an exclusive alternative to it",
    )
    .await;

    server.shutdown().await;
}

// ── ADR-212/Round 2: SAE J2534-2 clause 7 Additional Channels (`_CHx`)
// support, extended to Single Wire CAN -- mirrors `fault_tolerant_can.rs`'s
// own structure, adapted to SW-CAN's own family-specific surface (ComParam
// translation, the mock's `IOCTL_SW_CAN_HS`/`_NS` handler) that FT-CAN had
// none of. ─────────────────────────────────────────────────────────────────

/// Creates (but does not connect) a CLL for a compound `_CHx`-suffixed
/// `protocol_name`, with no `dlc_pin_data` -- local to this file since
/// `harness.rs`'s own `create_cll`/`resource_with_protocol` only cover the
/// raw-id route, mirroring `fault_tolerant_can.rs`'s own
/// `resource_with_protocol_name`/`create_cll` pair.
async fn create_cll_for_protocol_name(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    name: &str,
) -> ComLogicalLinkHandle {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolName(name.to_string())),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

/// A directly-named `PROTOCOL_SW_CAN_CAN_CH1` id establishes its own
/// physical channel independently of a `PROTOCOL_SW_ISO15765_PS` sibling on
/// the same module -- confirms `resources::chx_block_base`'s new
/// `PROTOCOL_SW_CAN_PS => Some(PROTOCOL_SW_CAN_CAN_CH1)` entry resolves end
/// to end, that a `_CHx` id (no J1962 pin concept at all) connects with no
/// `dlc_pin_data`, and -- critically -- that the second connect opens a
/// native `PROTOCOL_SW_CAN_CAN_CH1` channel, not a plain `PROTOCOL_CAN_CH1`
/// id (the discriminating assertion `base_protocol_id`'s ADR-211 recursion
/// fix, inherited unchanged by this round, must still satisfy). Mirrors
/// `fault_tolerant_can.rs`'s own
/// `ft_can_ch1_establishes_independently_alongside_a_ft_can_ps_sibling`.
#[tokio::test]
#[serial]
async fn sw_can_ch1_establishes_independently_alongside_a_sw_can_ps_sibling() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _ps_cll = create_and_connect_cll_for_resource_id(
        &mut client,
        SW_ISO15765_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, SWCAN_BAUD_RATE)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_SW_ISO15765_PS
    );

    let chx_cll = create_cll(&mut client, j2534_0404::PROTOCOL_SW_CAN_CAN_CH1, 2).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(chx_cll),
        })
        .await
        .expect(
            "connect_com_logical_link should succeed for a directly-named \
             PROTOCOL_SW_CAN_CAN_CH1 id",
        );

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the _PS sibling and its own _CH1 Additional Channel should open two distinct physical \
         channels"
    );
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID + 1),
        j2534_0404::PROTOCOL_SW_CAN_CAN_CH1,
        "the second connect should open a native PROTOCOL_SW_CAN_CAN_CH1 channel, not a plain \
         PROTOCOL_CAN_CH1 id -- this is the assertion that would have failed without \
         base_protocol_id's ADR-211 recursion fix feeding chx_block_base a CAN-collapse-keyed \
         block"
    );

    server.shutdown().await;
}

/// Connecting via the compound `_CHx`-suffixed `protocol_name` grammar
/// (`"ISO_11898_RAW_SWCAN_CH1"`, the `SW_CAN_PS` resource row's own
/// `protocol_name` with a `_CH1` suffix) succeeds, and -- critically --
/// resolves to the native `PROTOCOL_SW_CAN_CAN_CH1` id, not a plain
/// `PROTOCOL_CAN_CH1` id. This is the test that would fail without the
/// `names.rs` `requested_index.is_some()` bypass this round adds to SW-CAN's
/// own `resolve_pin_selection` arm: without it, this connect is rejected
/// outright as "mutually exclusive" (clause 6 Pin Selection vs. clause 7
/// Additional Channel) -- the exact masking gap ADR-212's Context section
/// describes. Mirrors `fault_tolerant_can.rs`'s own
/// `ft_can_connecting_via_compound_chx_name_succeeds_with_the_correct_native_id`.
#[tokio::test]
#[serial]
async fn sw_can_connecting_via_compound_chx_name_succeeds_with_the_correct_native_id() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll_for_protocol_name(&mut client, "ISO_11898_RAW_SWCAN_CH1").await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "connect_com_logical_link should succeed for the compound _CH1-suffixed name, not \
             be rejected as combining Pin Selection with an Additional Channel",
        );

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_SW_CAN_CAN_CH1,
        "the compound-name route must open a native PROTOCOL_SW_CAN_CAN_CH1 channel -- a plain \
         PROTOCOL_CAN_CH1 here would mean resolve_channel_selection silently downgraded the \
         caller's SW intent"
    );

    server.shutdown().await;
}

/// Same as above, for the ISO15765-based SW sibling
/// (`"ISO_15765_2_SWCAN_CH1"`) -- confirms the fix holds for both of
/// `chx_block_base`'s new SW-CAN entries, not just the raw-CAN one, and that
/// the vendor-header naming quirk (both block constants prefixed `SW_CAN_`
/// regardless of which `_PS` id they extend, ADR-212 Context item 2) is
/// encoded correctly.
#[tokio::test]
#[serial]
async fn sw_iso15765_connecting_via_compound_chx_name_succeeds_with_the_correct_native_id() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll_for_protocol_name(&mut client, "ISO_15765_2_SWCAN_CH1").await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "connect_com_logical_link should succeed for the compound _CH1-suffixed name, not \
             be rejected as combining Pin Selection with an Additional Channel",
        );

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH1,
        "the compound-name route must open a native PROTOCOL_SW_CAN_ISO15765_CH1 channel, not a \
         plain PROTOCOL_ISO15765_CH1 id"
    );

    server.shutdown().await;
}

/// A directly-named `PROTOCOL_SW_CAN_ISO15765_CH1` id is found by a
/// `GetResourceStatus` query using that SAME raw `_CHx` id -- mirrors
/// `fault_tolerant_can.rs`'s own
/// `get_resource_status_with_the_raw_ft_chx_id_finds_a_connected_chx_link`:
/// `rpc_link.rs`'s query-resolution `None if resources::is_sw_family_
/// protocol_id(id) ...` branch and its sibling `matches_status_hw_id`
/// skip-guard re-keying must both correctly recognize a raw `_CHx` SW-CAN
/// query id.
#[tokio::test]
#[serial]
async fn get_resource_status_with_the_raw_sw_can_chx_id_finds_a_connected_chx_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH1, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "connect_com_logical_link should succeed for a directly-named \
             PROTOCOL_SW_CAN_ISO15765_CH1 id",
        );
    assert_eq!(server.backdoor.connect_count(), 1);

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH1,
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();

    let resource_status_data = response
        .resource_status
        .expect("response should carry a ResourceStatusItem")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_status & 0x01,
        1,
        "querying the raw PROTOCOL_SW_CAN_ISO15765_CH1 id directly must report bit 0 (in use) \
         when the matching _CHx-connected SW link is actually connected, not silently fail to \
         match it and report idle"
    );

    server.shutdown().await;
}

/// Regression test for `rpc_link.rs`'s `apply_fd_mode` re-keying (ADR-212):
/// staging FD ComParams (`CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate`) on a
/// CLL resolved to a directly-named `PROTOCOL_SW_CAN_CAN_CH1` id and then
/// connecting is rejected outright, the same as this file's own
/// `staging_fd_comparams_on_an_sw_link_is_rejected_not_substituted` already
/// proves for the `_PS` case -- without the `is_sw_protocol_id` ->
/// `is_sw_family_protocol_id` re-keying, `base_protocol_id`'s recursion
/// (unchanged by this round) would let a `_CHx`-connected SW link's
/// `hw_protocol_id` resolve to plain `CAN` for `apply_fd_mode`'s own outer
/// match, and the narrower `is_sw_protocol_id` guard would then never
/// recognize `PROTOCOL_SW_CAN_CAN_CH1` as SW at all -- silently promoting it
/// to `FD_CAN_PS` instead of rejecting it. Mirrors
/// `fault_tolerant_can.rs`'s own
/// `staging_fd_comparams_on_a_chx_connected_ft_link_is_rejected_not_substituted`.
#[tokio::test]
#[serial]
async fn staging_fd_comparams_on_a_chx_connected_sw_link_is_rejected_not_substituted() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::PROTOCOL_SW_CAN_CAN_CH1, 1).await;
    set_com_param_unum32(
        &mut client,
        cll_handle,
        j2534_0404::DATA_RATE,
        SWCAN_BAUD_RATE,
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_BAUDRATE, 2_000_000).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "connecting a PROTOCOL_SW_CAN_CAN_CH1-resolved CLL with FD ComParams staged must be \
             rejected, not silently substituted to FD_CAN_PS",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("Single Wire CAN"),
        "the rejection should name the SWCAN-vs-FD conflict: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejection must happen before any native PassThruConnect is attempted"
    );

    server.shutdown().await;
}

/// `CP_ChangeSpeedRate` still translates to native `CONFIG_SW_CAN_HS_DATA_RATE`
/// on a `_CHx`-connected SW-CAN link -- regression test for `comparam_id.rs`'s
/// `to_j2534_config_id` re-keying (ADR-212 Decision item 3) from
/// `is_sw_protocol_id` to `is_sw_family_protocol_id`. Mirrors this file's own
/// `_PS`-level `change_speed_comparams_round_trip_and_the_mapped_three_reach_
/// native_config`'s `CONFIG_SW_CAN_HS_DATA_RATE` assertion, on a directly-named
/// `PROTOCOL_SW_CAN_CAN_CH1` link instead of `SW_ISO15765_PS`.
#[tokio::test]
#[serial]
async fn change_speed_rate_comparam_reaches_native_config_on_a_chx_connected_sw_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::PROTOCOL_SW_CAN_CAN_CH1, 1).await;
    set_com_param_unum32(
        &mut client,
        cll_handle,
        j2534_0404::DATA_RATE,
        SWCAN_BAUD_RATE,
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_CHANGE_SPEED_RATE, 83_300).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "connect_com_logical_link should succeed for a directly-named \
             PROTOCOL_SW_CAN_CAN_CH1 id with CP_ChangeSpeedRate staged",
        );

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_SW_CAN_HS_DATA_RATE),
        83_300,
        "CP_ChangeSpeedRate should still forward to CONFIG_SW_CAN_HS_DATA_RATE on a \
         _CHx-connected SW-CAN link, not silently drop the translation"
    );

    server.shutdown().await;
}

/// `PDU_IOCTL_SW_CAN_HS` succeeds on a `_CHx`-connected SW-CAN link --
/// regression test for the mock's own `IOCTL_SW_CAN_HS`/`_NS` handler
/// re-keying (ADR-212 Decision item 7) from the narrow `is_sw_protocol` to
/// the family-wide `is_sw_family_protocol`. Before this round's mock fix,
/// this would have returned `ERR_NOT_SUPPORTED`/`Unimplemented` -- mirrors
/// this file's own `_PS`-level
/// `sw_can_hs_ns_ioctls_succeed_on_sw_link_and_reject_on_non_sw_link`'s
/// success assertion shape, on a directly-named `PROTOCOL_SW_CAN_CAN_CH1`
/// link instead.
#[tokio::test]
#[serial]
async fn sw_can_hs_ioctl_succeeds_on_a_chx_connected_sw_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::PROTOCOL_SW_CAN_CAN_CH1, 1).await;
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
        .expect(
            "connect_com_logical_link should succeed for a directly-named \
             PROTOCOL_SW_CAN_CAN_CH1 id",
        );
    assert_eq!(server.backdoor.connect_count(), 1);

    let hs_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SW_CAN_HS").await;
    io_ctl_cll(&mut client, cll_handle, hs_id).await.expect(
        "PDU_IOCTL_SW_CAN_HS should succeed on a _CHx-connected SW-CAN CLL, not be rejected \
             as ERR_NOT_SUPPORTED",
    );
    assert_eq!(
        server.backdoor.sw_can_hs_count(),
        1,
        "the native SW_CAN_HS IOCTL should have reached the mock exactly once"
    );

    server.shutdown().await;
}

/// `CP_SwCan_HighVoltage` set nonzero still causes `TX_FLAG_SW_CAN_HV_TX` to
/// reach a subsequent `CoptSendrecv` on a `_CHx`-connected SW-CAN link --
/// regression test for a gap found (not by the up-front investigation, but
/// during this round's own implementation/verification pass) in TWO
/// call sites that gate this bit on the narrow `is_sw_protocol_id`:
/// `rpc_primitive::apply_resolved_tx_flags` (the shared TX-flags composition
/// this `CoptSendrecv` funnels through) and `rpc_misc.rs`'s own, independent
/// Repeat-Message TX-flags composition. Both re-keyed to
/// `is_sw_family_protocol_id` as part of this round (ADR-212 Decision item
/// 4's own class of fix, discovered late rather than up front -- mirrors
/// this file's own `sw_can_high_voltage_comparam_sets_tx_flag_on_an_sw_link`
/// (the `_PS` case) exactly, on a directly-named `PROTOCOL_SW_CAN_CAN_CH1`
/// link instead.
#[tokio::test]
#[serial]
async fn sw_can_high_voltage_comparam_sets_tx_flag_on_a_chx_connected_sw_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::PROTOCOL_SW_CAN_CAN_CH1, 1).await;
    set_com_param_unum32(
        &mut client,
        cll_handle,
        j2534_0404::DATA_RATE,
        SWCAN_BAUD_RATE,
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_SW_CAN_HIGH_VOLTAGE, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "connect_com_logical_link should succeed for a directly-named \
             PROTOCOL_SW_CAN_CAN_CH1 id with CP_SwCan_HighVoltage staged",
        );
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    send_data(&mut client, cll_handle, vec![0x01, 0x02], vec![]).await;
    assert_ne!(
        server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0) & j2534_0404::TX_FLAG_SW_CAN_HV_TX,
        0,
        "a nonzero CP_SwCan_HighVoltage should set TX_FLAG_SW_CAN_HV_TX on a _CHx-connected \
         SW-CAN link's send, not silently drop the bit the way the narrow, pre-ADR-212 \
         is_sw_protocol_id gate would have"
    );

    server.shutdown().await;
}

/// The RX-side mirror of the TX-side gap the test just above regresses:
/// `SW_CAN_HV_RX` (RxStatus bit 16, ISO 22900-2 Table D.5 byte 1 bit 0) must
/// still tag a `_CHx`-connected SW-CAN link's `RxFlag`, not just a `_PS`
/// one -- found by an `edge-case-hunter` pass on this round's own diff
/// (reproduced there, then fixed here): `events.rs`'s `sw_can_hv_rx`
/// computation was re-keyed everywhere else in this same withholding block
/// EXCEPT this one binding, which stayed on the narrow `is_sw_protocol_id`.
/// Mirrors this file's own `_PS`-level
/// `sw_can_hv_rx_bit_is_not_withheld_and_is_delivered_as_content` exactly,
/// on a directly-named `PROTOCOL_SW_CAN_CAN_CH1` link instead.
#[tokio::test]
#[serial]
async fn sw_can_hv_rx_bit_is_delivered_with_the_rxflag_tag_on_a_chx_connected_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::PROTOCOL_SW_CAN_CAN_CH1, 1).await;
    set_com_param_unum32(
        &mut client,
        cll_handle,
        j2534_0404::DATA_RATE,
        SWCAN_BAUD_RATE,
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_REQUEST_ADDR_MODE, 2).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CAN_FUNC_REQ_ID, 0x7DF).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for a directly-named PROTOCOL_SW_CAN_CAN_CH1 id");
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let payload = vec![0x01, 0x02, 0x03];
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &can_frame(0x123, &payload),
        j2534_0404::PROTOCOL_SW_CAN_CAN_CH1,
        RX_STATUS_SW_CAN_HV_RX,
    );

    let result = wait_for_result_data(&mut client, cll_handle).await;
    assert_result_data_with_rx_flag(
        &result,
        &0x123_u32.to_be_bytes(),
        &[],
        &payload,
        &[0x00, 0x01, 0x00, 0x00],
    );

    server.shutdown().await;
}

/// Capacity-discrimination test, mirroring `fault_tolerant_can.rs`'s own
/// `connect_rejects_a_chx_index_within_the_generic_capacity_but_above_ft_cans_own`
/// EXACTLY (see that test's own doc comment for why this exact shape is the
/// one that's genuinely discriminating -- a plain "rejects out-of-range
/// index" test is NOT discriminating on its own, since the mock's generic
/// `chx_capacity` alone can't tell "correctly consulted SW-CAN's own flag"
/// apart from "wrongly consulted the generic CAN flag"). Uses the new,
/// independent `__mock_set_sw_can_chx_capacity` override (ADR-212 Decision
/// item 7).
#[tokio::test]
#[serial]
async fn connect_rejects_a_chx_index_within_the_generic_capacity_but_above_sw_cans_own() {
    let server = start_j2534_2_server().await;
    server.backdoor.set_chx_capacity(10);
    server.backdoor.set_sw_can_chx_capacity(1);
    let mut client = server.client().await;

    let cll_handle = create_cll(
        &mut client,
        j2534_0404::PROTOCOL_SW_CAN_CAN_CH1 + 4, // _CH5
        1,
    )
    .await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "_CH5 is within the generic families' cached capacity (10) but exceeds Single Wire \
             CAN's own (1) -- must be rejected synchronously by check_chx_capacity consulting \
             DEVICE_INFO_SW_CAN_SUPPORTED, not DEVICE_INFO_CAN_SUPPORTED",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the capacity precheck must reject before any native PassThruConnect is attempted -- if \
         this fires, check_chx_capacity consulted the generic (higher) capacity instead of \
         Single Wire CAN's own, exactly the bug this fix would be"
    );

    server.shutdown().await;
}

//! End-to-end coverage for SAE J2534-2 clause 21 CAN FD's connect-time
//! protocol substitution (ADR-158, Phase 3a "core connect mechanics") through
//! the full gRPC stack: staged Working ComParams (`CP_CANFDTxMaxDataLength`/
//! `CP_CANFDBaudrate`) driving `ConnectComLogicalLink`'s internal
//! `PassThruIoctl(SET_CONFIG, CONFIG_FD_CAN_DATA_PHASE_RATE)` ->
//! `PassThruConnect(FD_CAN_PS)` -> `PassThruIoctl(SET_CONFIG,
//! CONFIG_J1962_PINS)` sequence -- invisible to the client on success, so
//! these tests use `server.backdoor.channel_protocol_id`/`config_value`/
//! `set_config_param_log` to observe the mock's actual channel state, the
//! same technique `pin_selection.rs` uses for its own internal
//! `_PS`/`PassThruIoctl` sequence. Mirrors `pin_selection.rs`'s/
//! `additional_channels.rs`'s structure and helpers.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DataItem, DisconnectComLogicalLinkRequest, ExpectedResponseData,
    GetObjectIdRequest, GetResourceStatusRequest, IoCtlRequest, IoFilter, IoFilterList,
    LockResourceRequest, ModuleAndResourceId, ModuleHandle, ObjectType, ParamItem, PduError,
    PduFilter, PinData, ResourceData, SetComParamRequest, StartComPrimitiveRequest,
    UnlockResourceRequest, create_com_logical_link_request, data_item, error_detail_from_status,
    event_item, io_ctl_request, module_and_resource_id, param_item, pin_data, resource_data,
    subscribe_event_request, vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)` -- same per-file local helper shape as
/// `pdu_ioctl.rs`'s/`repeat_message.rs`'s own (not shared via `harness.rs`,
/// matching this codebase's existing convention).
async fn resolve_ioctl_id(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    name: &str,
) -> u32 {
    client
        .get_object_id(GetObjectIdRequest {
            object_type: ObjectType::ObjtIoCtrl as i32,
            shortname: name.to_string(),
        })
        .await
        .unwrap_or_else(|err| panic!("get_object_id({name}) should succeed: {err}"))
        .into_inner()
        .pdu_object_id
}

/// Installs a single client filter (all-zero mask/pattern) on `cll_handle`
/// via `PDU_IOCTL_START_MSG_FILTER` -- mirrors `pdu_ioctl.rs`'s
/// `start_msg_filter_installs_once_per_tx_flags_variant_on_a_can_id_both_channel`
/// pattern. Uses `PDU_FLT_BLOCK`, not `PDU_FLT_PASS`: this adapter rejects a
/// client `PDU_FLT_PASS`/`PDU_FLT_PASS_UUDT` on a raw-CAN link outright (it
/// already keeps its own pass-all hardware baseline so its response matching
/// sees every frame), the same constraint `pdu_ioctl.rs`'s own precedent
/// works around by using `PduFltBlock`.
async fn install_client_pass_filter(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
) {
    let start_filter_id = resolve_ioctl_id(client, "PDU_IOCTL_START_MSG_FILTER").await;
    client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                start_filter_id,
            )),
            input_data: Some(DataItem {
                data: Some(data_item::Data::FilterData(IoFilterList {
                    filters: vec![IoFilter {
                        filter_type: PduFilter::PduFltBlock as i32,
                        filter_number: 1,
                        filter_mask_message: vec![0, 0, 0, 0],
                        filter_pattern_message: vec![0, 0, 0, 0],
                    }],
                })),
            }),
            has_output: false,
        })
        .await
        .expect("PDU_IOCTL_START_MSG_FILTER should succeed");
}

/// Waits for the next `PduCopstFinished` on an already-open event stream
/// (must be subscribed BEFORE the COP being waited on is started) -- same
/// per-file helper shape as `param_binding.rs`'s/`comparam_tx.rs`'s own.
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

/// Builds a `RscData` resource directly naming `raw_protocol_id` via the
/// unambiguous `ProtocolId` route (ADR-178: the removed `channel_index`
/// field route's replacement) -- mirrors `additional_channels.rs`'s
/// identical helper.
fn resource_with_protocol_id(raw_protocol_id: u32) -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![],
        bus_type: None,
        protocol: Some(resource_data::Protocol::ProtocolId(raw_protocol_id)),
    }
}

/// Builds a `RscData` resource selecting `protocol_id` via the unambiguous
/// `ProtocolId` route with the given typed `(pin_number, pin_type_name)`
/// pairs as `dlc_pin_data` -- identical shape to
/// `pin_selection.rs`'s own `resource_with_protocol_id_and_pins`.
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

/// Same helper shape as `pin_selection.rs`'s/`additional_channels.rs`'s
/// `try_create_cll_with_resource`.
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

/// Regression test: leaving `CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate` at
/// their defaults (`0`) must produce a plain Classic CAN connect,
/// byte-for-byte the pre-ADR-158 behavior -- no `CONFIG_FD_CAN_DATA_PHASE_RATE`
/// SET_CONFIG, no `CONFIG_J1962_PINS` SET_CONFIG, `PassThruConnect` uses
/// plain `PROTOCOL_CAN`, and the module needs no J2534-2 opt-in at all (the
/// FD-mode gate never runs unless `fd_mode` is actually true).
#[tokio::test]
#[serial]
async fn classic_can_defaults_connect_unchanged() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let baseline_set_config = server.backdoor.set_config_count();

    let _cll = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::CAN,
        "leaving CAN FD ComParams at their defaults must connect as plain CAN"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_FD_CAN_DATA_PHASE_RATE),
        0,
        "no FD data-phase-rate SET_CONFIG should have run"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0,
        "no pins SET_CONFIG should have run for an unqualified Classic CAN connect"
    );
    assert_eq!(
        server.backdoor.set_config_count(),
        baseline_set_config,
        "no SET_CONFIG call of any kind should happen for this connect"
    );

    server.shutdown().await;
}

/// End-to-end FD_CAN connect: staging `CP_CANFDTxMaxDataLength`/
/// `CP_CANFDBaudrate` on an opted-in module makes `ConnectComLogicalLink`
/// connect with the native `PROTOCOL_FD_CAN_PS` id, issue
/// `SET_CONFIG(CONFIG_FD_CAN_DATA_PHASE_RATE)` BEFORE
/// `SET_CONFIG(CONFIG_J1962_PINS)` (clause 21.3.2.5.1's mandatory ordering --
/// the mock's own sequencing enforcement makes a wrong order fail the
/// connect outright), and assign the base protocol's own default packed pins
/// (`0x0000060E`, CAN's default 6/HI-14/LOW) since no `dlc_pin_data` was
/// supplied.
#[tokio::test]
#[serial]
async fn fd_can_connect_applies_data_phase_rate_before_pins() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
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
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "staged FD ComParams should substitute the native connect id to FD_CAN_PS"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_FD_CAN_DATA_PHASE_RATE),
        2_000_000,
        "the data phase rate should be CP_CANFDBaudrate (nonzero, so it wins over CP_Baudrate)"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_060E,
        "an unqualified FD connect (no dlc_pin_data) should assign CAN's own default packed \
         pins (6/HI, 14/LOW)"
    );

    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    let rate_index = log
        .iter()
        .position(|&p| p == j2534_0404::CONFIG_FD_CAN_DATA_PHASE_RATE)
        .expect("CONFIG_FD_CAN_DATA_PHASE_RATE should have been SET_CONFIG'd");
    let pins_index = log
        .iter()
        .position(|&p| p == j2534_0404::CONFIG_J1962_PINS)
        .expect("CONFIG_J1962_PINS should have been SET_CONFIG'd");
    assert!(
        rate_index < pins_index,
        "CONFIG_FD_CAN_DATA_PHASE_RATE must be SET_CONFIG'd before CONFIG_J1962_PINS (clause \
         21.3.2.5.1); log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Mode is recomputed fresh on every connect, never sticky: connecting as
/// FD, disconnecting, clearing the FD-triggering ComParam, and reconnecting
/// must now be a plain Classic CAN connect on a brand-new physical channel
/// (the native id itself differs, so `ChannelKey` differs too).
#[tokio::test]
#[serial]
async fn reconnect_after_clearing_fd_comparams_flips_back_to_classic_can() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
        ],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS
    );

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 0).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("reconnecting after clearing the FD ComParam should succeed");

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the reconnect's differing native protocol id (CAN vs FD_CAN_PS) should open a \
         brand-new physical channel, not rejoin the FD one"
    );
    const RECONNECT_CHANNEL_ID: u32 = 2;
    assert_eq!(
        server.backdoor.channel_protocol_id(RECONNECT_CHANNEL_ID),
        j2534_0404::CAN,
        "clearing the FD-triggering ComParam must flip the reconnect back to plain CAN"
    );

    server.shutdown().await;
}

/// `CP_CANFDTxMaxDataLength == 8` alone (Classic CAN's own max payload) is
/// ambiguous and must NOT trigger FD mode -- but paired with a nonzero
/// `CP_CANFDBaudrate`, the baudrate alone is decisive (ADR-158's trigger
/// rule: `TX_DL > 8 || baudrate != 0`).
#[tokio::test]
#[serial]
async fn tx_dl_eight_alone_does_not_trigger_but_with_baudrate_does() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let _cll_a = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 8),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::CAN,
        "TX_DL == 8 alone (Classic CAN's own max payload) must not trigger FD mode"
    );

    const CHANNEL_B: u32 = 2;
    let _cll_b = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 250_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 8),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(CHANNEL_B),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "TX_DL == 8 paired with a nonzero CP_CANFDBaudrate must trigger FD mode -- the \
         baudrate alone is decisive"
    );

    server.shutdown().await;
}

/// FD mode requested on a module that has NOT opted into J2534-2 (its
/// `pname` lacks the `"J2534-2:"` prefix, clause 5) is rejected with
/// `invalid_argument` at `ConnectComLogicalLink` time -- `CreateComLogicalLink`
/// itself succeeds (staging ComParams is not gated), mirroring
/// `pin_selection.rs`'s equivalent opt-in gate.
#[tokio::test]
#[serial]
async fn fd_mode_rejected_at_connect_when_module_not_opted_in() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll_for_module(&mut client, 1, j2534_0404::CAN, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err("a non-opted-in module must reject an FD-mode connect");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejection must happen before any native PassThruConnect is attempted"
    );

    server.shutdown().await;
}

/// ADR-213/Round 3 core promotion test: FD mode combined with a
/// directly-named `_CHx`-qualified link (SAE J2534-2 clause 7 Additional
/// Channels) now PROMOTES to the corresponding native `FD_CAN_CHx` id,
/// instead of being rejected -- this supersedes ADR-158 Decision item 1's
/// `channel_index.is_some()` rejection (this test used to assert that
/// rejection; ADR-213 Decision item 1 replaces it with index-aware
/// promotion via `resources::fd_protocol_id_for_link`). `CreateComLogicalLink`
/// itself succeeds (the `_CHx` id alone resolves normally, unaffected by
/// whether FD ComParams are staged); the promotion happens only once
/// `ConnectComLogicalLink` is attempted.
#[tokio::test]
#[serial]
async fn fd_mode_combined_with_a_directly_named_chx_id_promotes_to_fd_can_chx() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_CAN_CH1 + 2), // _CH3
        1,
    )
    .await
    .expect("a directly-named _CH3 id alone should resolve to a _CHx hardware variant for CAN");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect(
            "FD mode combined with a _CHx-qualified link should now promote (ADR-213), not be \
             rejected",
        );

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_CH1 + 2,
        "the connect should substitute to the native FD_CAN_CH3 id, not plain CAN_CH3 or bare CAN"
    );

    server.shutdown().await;
}

/// ADR-213/Round 3 regression test (edge-case-hunter finding, PR review --
/// only manually traced, not pinned with a test, in the original round): a
/// `GetResourceStatus` query for the raw `PROTOCOL_FD_CAN_CH3` id must find
/// a link that was connected as `CAN_CH3` and then promoted to `FD_CAN_CH3`
/// by staging FD ComParams -- `rpc_get_resource_status`'s own query-candidate
/// matching resolves the queried id's `_CHx` index via `chx_base_protocol_id`
/// (widened for CAN FD by this round) and falls through to a
/// `base_hw_protocol_id() == CAN` comparison, which is satisfied by an FD
/// `_CHx` link at the matching index the same way it already is by a plain
/// classic `_CHx` link -- confirmed correct by direct code tracing, this
/// test pins that reasoning with an actual regression rather than leaving it
/// resting on manual analysis alone.
#[tokio::test]
#[serial]
async fn get_resource_status_with_the_raw_fd_can_chx_id_finds_a_promoted_chx_link() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_CAN_CH1 + 2), // _CH3
        1,
    )
    .await
    .expect("a directly-named _CH3 id alone should resolve to a _CHx hardware variant for CAN");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("FD mode combined with a _CHx-qualified link should promote (ADR-213)");
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_CH1 + 2,
        "sanity: the link must actually be FD-promoted for this query to have anything meaningful \
         to find"
    );

    let response = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle { module_handle: 1 }),
                resource: Some(module_and_resource_id::Resource::ResourceId(
                    j2534_0404::PROTOCOL_FD_CAN_CH1 + 2,
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
        "querying the raw PROTOCOL_FD_CAN_CH3 id directly must report bit 0 (in use) when the \
         matching FD-promoted _CHx link is actually connected, not silently fail to match it and \
         report idle"
    );

    server.shutdown().await;
}

/// ADR-213/Round 3 reversion test: reconnecting a `_CHx` FD link after its
/// ComParams stop signaling FD must land back on the SAME `CAN_CH<n>` id it
/// started from, not the bare base `CAN` -- the exact bug class ADR-213's own
/// Consequences section flags as needing dedicated coverage (mirrors the
/// same class of gap ADR-211's own final-diff `edge-case-hunter` sweep
/// caught for FT-CAN). Mirrors `reconnect_after_clearing_fd_comparams_flips_
/// back_to_classic_can` above, but for a `_CHx`-connected link.
#[tokio::test]
#[serial]
async fn reconnect_after_clearing_fd_comparams_on_a_chx_link_flips_back_to_can_chx_not_bare_can() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_CAN_CH1 + 2), // _CH3
        1,
    )
    .await
    .expect("a directly-named _CH3 id alone should resolve to a _CHx hardware variant for CAN");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect should succeed and promote to FD_CAN_CH3");
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_CH1 + 2
    );

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 0).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("reconnecting after clearing the FD ComParam should succeed");

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the reconnect's differing native protocol id (CAN_CH3 vs FD_CAN_CH3) should open a \
         brand-new physical channel, not rejoin the FD one"
    );
    const RECONNECT_CHANNEL_ID: u32 = 2;
    assert_eq!(
        server.backdoor.channel_protocol_id(RECONNECT_CHANNEL_ID),
        j2534_0404::PROTOCOL_CAN_CH1 + 2,
        "clearing the FD-triggering ComParam on a _CHx link must revert to that SAME CAN_CH3 id \
         -- ADR-213's three-way revert -- not bare CAN, which is the bug this test guards against"
    );

    server.shutdown().await;
}

/// ADR-213/Round 3 capacity-cap discriminating test, mirroring
/// `fault_tolerant_can.rs`'s own `connect_rejects_a_chx_index_within_the_
/// generic_capacity_but_above_ft_cans_own`: a `_CH5` classic-CAN link staging
/// FD ComParams promotes to `FD_CAN_CH5` before `check_chx_capacity` runs
/// (`apply_fd_mode` runs before the `j2534_proto_id` snapshot that feeds it),
/// so the precheck must consult `DEVICE_INFO_FD_CAN_SUPPORTED`'s own
/// dedicated capacity, not the generic families' cached one.
#[tokio::test]
#[serial]
async fn connect_rejects_a_chx_index_within_the_generic_capacity_but_above_fd_cans_own() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    server.backdoor.set_chx_capacity(10);
    server.backdoor.set_fd_can_chx_capacity(1);
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_CAN_CH1 + 4), // _CH5
        1,
    )
    .await
    .expect("a directly-named _CH5 id alone should resolve to a _CHx hardware variant for CAN");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "_CH5 is within the generic families' cached capacity (10) but exceeds CAN FD's own \
             (1) -- must be rejected synchronously by check_chx_capacity consulting \
             DEVICE_INFO_FD_CAN_SUPPORTED, not DEVICE_INFO_CAN_SUPPORTED",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the capacity precheck must reject before any native PassThruConnect is attempted"
    );

    server.shutdown().await;
}

/// ADR-213/Round 3: a directly-named raw `FD_CAN_CH1` id (no ComParams
/// staged) is still rejected at `CreateComLogicalLink` time with the same
/// "FD mode is derived from staged ComParams" message (now mentioning
/// `_CHx`, section 4) -- proves the `names.rs` message-text-only change did
/// not also relax the actual rejection logic, and that the widened
/// `is_fd_protocol_id` (section 2b) correctly still catches a `_CHx` id
/// here. Mirrors `direct_fd_can_protocol_id_naming_is_rejected` above, for
/// the `_CHx` case.
#[tokio::test]
#[serial]
async fn direct_fd_can_chx_protocol_id_naming_is_rejected() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let status = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_FD_CAN_CH1),
        1,
    )
    .await
    .expect_err("a directly-named FD_CAN_CH1 protocol id must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("_CHx"),
        "the rejection message should now mention _CHx as well as _PS: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejection must happen at CreateComLogicalLink, before any connect is attempted"
    );

    server.shutdown().await;
}

/// ADR-213 Decision item 3: a `_CHx`-connected FD link's connect must NOT
/// issue `SET_CONFIG(CONFIG_J1962_PINS)` -- unlike an unqualified `_PS` FD
/// connect (`fd_can_connect_applies_data_phase_rate_before_pins` above,
/// which DOES synthesize a default pin assignment since clause 21 has no
/// `_PS` default), a `_CHx` channel is already vendor-pin-preassigned per
/// clause 21.3.2.5.1, so issuing that SET_CONFIG would be a spec violation.
/// The data-phase-rate SET_CONFIG must still fire, unconditionally, since
/// that is the step that attaches a `_CHx` channel.
#[tokio::test]
#[serial]
async fn fd_can_chx_connect_does_not_synthesize_j1962_pins() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id(j2534_0404::PROTOCOL_CAN_CH1 + 2), // _CH3
        1,
    )
    .await
    .expect("a directly-named _CH3 id alone should resolve to a _CHx hardware variant for CAN");
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect should succeed and promote to FD_CAN_CH3");

    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_CH1 + 2
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_FD_CAN_DATA_PHASE_RATE),
        "the data-phase rate SET_CONFIG must still fire for a _CHx FD connect -- it's the step \
         that attaches the channel per clause 21.3.2.5.1"
    );
    assert!(
        !log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "a _CHx FD connect must NOT synthesize a CONFIG_J1962_PINS call -- the channel is \
         already vendor-pin-preassigned"
    );

    server.shutdown().await;
}

/// FD mode combined with software-ISO-TP mode (`can_channel_mode =
/// "software-isotp"`, ADR-046) is rejected at `ConnectComLogicalLink` time --
/// no software-ISO-TP extension for CAN-FD-sized segmented messages this
/// phase.
#[tokio::test]
#[serial]
async fn fd_mode_rejected_when_link_is_software_isotp() {
    let server = TestServer::try_start_with_extra_config(&format!(
        "can_channel_mode = \"software-isotp\"\n{}",
        modules_toml(&[("Bench 1", "J2534-2:mock")])
    ))
    .await
    .expect("service should initialize with a valid modules + can_channel_mode config");
    let mut client = server.client().await;

    // ISO15765 is the protocol family software-ISO-TP mode actually applies
    // to (ADR-046) -- CAN FD's own base-protocol gate
    // (`base_hw_protocol_id() == CAN`) still triggers for it, since
    // `resources::base_protocol_id` treats an ISO15765 link's raw CAN
    // hardware channel (software_isotp substitutes CAN for the actual
    // PassThruConnect) the same way.
    let cll_handle = create_cll_for_module(&mut client, 1, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err("FD mode on a software-ISO-TP link must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// ADR-158 correction (Codex review, PR #30): a hardware-ISO15765 CLL (no
/// `can_channel_mode = "software-isotp"`, unlike
/// `fd_mode_rejected_when_link_is_software_isotp` above -- this CLL's own
/// `hw_protocol_id` is genuinely `ISO15765`, never substituted to `CAN`)
/// with `CP_CANFDBaudrate` staged on its Working set must be rejected at
/// `ConnectComLogicalLink` time, not silently connected as a classic
/// ISO15765 channel. `is_param_allowed`'s CAN-family gate
/// (`comparam_support.rs`) allow-lists this ComParam for `ISO15765` too, so
/// `SetComParam` itself must succeed -- the rejection belongs at connect
/// time, mirroring the loud-rejection precedent this ADR already
/// established for `channel_index`/software-ISO-TP.
#[tokio::test]
#[serial]
async fn fd_can_iso15765_link_with_staged_fd_comparams_is_rejected_not_silently_ignored() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll_for_module(&mut client, 1, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_BAUDRATE, 2_000_000).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "an FD ComParam staged on a hardware-ISO15765 link must be rejected, not silently \
             ignored",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejection must happen before any native PassThruConnect is attempted"
    );

    server.shutdown().await;
}

/// Companion to the test above: an ordinary hardware-ISO15765 CLL with no
/// FD ComParams staged at all must still connect normally as classic
/// ISO15765 -- the new base-protocol-family check must not introduce a
/// false-positive rejection for the overwhelmingly common case.
#[tokio::test]
#[serial]
async fn fd_can_iso15765_link_without_fd_comparams_connects_normally() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll_for_module(&mut client, 1, j2534_0404::ISO15765, 1).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 500_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("an ordinary ISO15765 link with no FD ComParams staged must connect normally");
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::ISO15765,
        "the channel must connect as plain ISO15765, unaffected by the new check"
    );

    server.shutdown().await;
}

/// ADR-158 correction (Codex review, PR #30, round 2): `same_physical_resource`'s
/// `channel_key` branch (used once BOTH sides are connected, e.g. by
/// `recompute_lock_tx_suspensions` after every lock grant/release) used to
/// compare the FULL `ChannelKey` including the FD data-phase rate, so two
/// `FD_CAN_PS` CLLs on the same explicit pins/arbitration rate but
/// different data-phase rates -- genuinely two distinct physical channels
/// by `ChannelKey` design, but still the same J1962 pins -- were wrongly
/// treated as unrelated resources for locking purposes. `cll_a` locks
/// `LOCK_PHYSICAL_TX_QUEUE` and `cll_b` (same pins, different data-phase
/// rate, connected to its own distinct channel) must still be suspended.
#[tokio::test]
#[serial]
async fn fd_can_lock_suspends_a_same_pins_different_rate_sibling_once_both_are_connected() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;
    const SECOND_CHANNEL_ID: u32 = 2;
    const PINS: &[(u32, &str)] = &[(3, "HI"), (11, "LOW")];

    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_a_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, PINS),
        1,
    )
    .await
    .expect("create_com_logical_link should succeed for cll_a's pin selection");
    set_com_param_unum32(&mut client, cll_a_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;
    set_com_param_unum32(&mut client, cll_a_handle, CP_CANFD_BAUDRATE, 2_000_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a_handle),
        })
        .await
        .expect("cll_a's connect_com_logical_link should succeed");

    // cll_b: same explicit pins as cll_a, same arbitration rate, but a
    // DIFFERENT effective data-phase rate -- opens its OWN physical channel
    // (ChannelKey's 4th element differs), since no lock is held yet at
    // connect time.
    let cll_b_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, PINS),
        2,
    )
    .await
    .expect("create_com_logical_link should succeed for cll_b's (identical) pin selection");
    set_com_param_unum32(&mut client, cll_b_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_b_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;
    set_com_param_unum32(&mut client, cll_b_handle, CP_CANFD_BAUDRATE, 5_000_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b_handle),
        })
        .await
        .expect("cll_b's connect_com_logical_link should succeed -- distinct data-phase rate opens its own channel, no lock is held yet");
    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "sanity: cll_a and cll_b must be on two distinct physical channels"
    );
    set_can_phys_req_id_and_promote(&mut client, cll_b_handle, 0x7E0).await;

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a_handle),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("lock_resource(LOCK_PHYSICAL_TX_QUEUE) should succeed on cll_a");

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b_handle),
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
        .expect("CoptSendrecv should be accepted (queued), not rejected");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    assert_eq!(
        server.backdoor.written_count(SECOND_CHANNEL_ID),
        0,
        "cll_b must be suspended by cll_a's LOCK_PHYSICAL_TX_QUEUE despite being on a distinct \
         physical channel -- same pins means same physical resource regardless of data-phase rate"
    );

    client
        .unlock_resource(UnlockResourceRequest {
            cll_handle: Some(cll_a_handle),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("unlock_resource should succeed");
    wait_for_written_count(&server, SECOND_CHANNEL_ID, 1).await;

    server.shutdown().await;
}

/// Companion to the test above, covering a THIRD `same_physical_resource`
/// call site the other one doesn't exercise: `rpc_lock_resource`'s own Fix C
/// (ADR-123) active-transmission scan, which runs at grant time (not via
/// `recompute_lock_tx_suspensions`, which only runs AFTER a grant). `cll_a`
/// (FD_CAN_PS, rate R1) has an actively-transmitting COP; `cll_b` (FD_CAN_PS,
/// same pins, DIFFERENT rate R2, its own distinct physical channel) attempts
/// `LockResource(LOCK_PHYSICAL_TX_QUEUE)` and must be rejected -- before the
/// round-2 fix, the differing data-phase rate made the two channels' full
/// `ChannelKey`s compare unequal, so this scan would never have found
/// `cll_a`'s active transmission and would have wrongly granted the lock.
#[tokio::test]
#[serial]
async fn fd_can_lock_resource_rejects_a_same_pins_different_rate_siblings_active_transmission() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;
    const PINS: &[(u32, &str)] = &[(3, "HI"), (11, "LOW")];

    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_a_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, PINS),
        1,
    )
    .await
    .expect("create_com_logical_link should succeed for cll_a's pin selection");
    set_com_param_unum32(&mut client, cll_a_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;
    set_com_param_unum32(&mut client, cll_a_handle, CP_CANFD_BAUDRATE, 2_000_000).await;
    // 500ms response window -- ample real time to observe the active
    // transmission from cll_b's LockResource attempt, mirroring
    // `locks_and_param_classes.rs`'s `lock_resource_rejects_active_transmission_conflict_with_fct_failed`.
    set_com_param_unum32(&mut client, cll_a_handle, j2534_0404::P2_MAX, 500_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a_handle),
        })
        .await
        .expect("cll_a's connect_com_logical_link should succeed");

    let cll_b_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, PINS),
        2,
    )
    .await
    .expect("create_com_logical_link should succeed for cll_b's (identical) pin selection");
    set_com_param_unum32(&mut client, cll_b_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_b_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;
    set_com_param_unum32(&mut client, cll_b_handle, CP_CANFD_BAUDRATE, 5_000_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b_handle),
        })
        .await
        .expect("cll_b's connect_com_logical_link should succeed -- distinct data-phase rate opens its own channel");
    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "sanity: cll_a and cll_b must be on two distinct physical channels"
    );
    // CoptSendrecv requires addressing to be configured first (CP_CanPhysReqId),
    // same as the sibling test above for cll_b.
    set_can_phys_req_id_and_promote(&mut client, cll_a_handle, 0x7E0).await;

    // cll_a's CoptSendrecv occupies the poll task inside its receive-phase
    // wait for (nearly) its whole 500ms CP_P2Max window -- nothing ever
    // answers, so `executing_cop` stays set to this COP the whole time.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a_handle),
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
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let status = client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_b_handle),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect_err(
            "lock_resource should be rejected: cll_a's active transmission on the same pins, \
             despite cll_a and cll_b having distinct FD data-phase rates and thus distinct \
             physical channels, must still count as the same physical resource",
        );
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrFctFailed as i32
    );

    server.shutdown().await;
}

/// ADR-158 correction (Codex review, PR #30, round 3): a Classic-connected
/// CLL that stages FD-triggering ComParams post-connect must have its
/// `CoptUpdateparam` rejected, not silently promoted -- `CP_CANFDTxMaxDataLength`/
/// `CP_CANFDBaudrate` have no native `SET_CONFIG` mapping, so nothing would
/// ever forward this request to hardware, and the channel would stay
/// Classic while `GetComParam` claimed FD was requested.
#[tokio::test]
#[serial]
async fn coptupdateparam_rejects_promoting_fd_comparams_on_a_classic_connected_link() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::CAN,
        "sanity: cll_handle must be Classic-connected for this scenario to apply"
    );

    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_BAUDRATE, 2_000_000).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "CoptUpdateparam promoting FD-triggering ComParams onto a Classic-connected link \
             must be rejected, not silently promoted",
        );
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Mirror-image direction of the test above: an FD-connected CLL that
/// stages ComParams no longer signaling FD post-connect must also have its
/// `CoptUpdateparam` rejected -- promoting would make `GetComParam` claim
/// Classic while the physical channel (and `ChannelKey`/lock comparisons/
/// `to_j2534_config_id`'s FD suppression) all still see FD.
#[tokio::test]
#[serial]
async fn coptupdateparam_rejects_promoting_classic_comparams_on_an_fd_connected_link() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: cll_handle must be FD-connected for this scenario to apply"
    );

    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 0).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_BAUDRATE, 0).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptUpdateparam as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect_err(
            "CoptUpdateparam promoting Classic-signaling ComParams onto an FD-connected link \
             must be rejected, not silently promoted",
        );
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Companion to the two rejection tests above: an FD-connected CLL
/// promoting a DIFFERENT `CP_CANFDBaudrate` value that is STILL nonzero
/// (still signals FD, just a different rate) must still succeed -- the
/// ADR-158 accepted residual this correction deliberately does NOT close
/// (connect-latched data-phase rate; the promoted value has no hardware
/// round-trip check, same class as `CP_Baudrate`/ADR-011). Confirms the new
/// guard rejects only a signal CROSSING the FD/Classic boundary, not every
/// within-mode value change.
#[tokio::test]
#[serial]
async fn coptupdateparam_promoting_an_unrelated_param_on_an_fd_connected_link_still_succeeds() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: cll_handle must be FD-connected for this scenario to apply"
    );

    // Still nonzero -- fd_mode_staged stays true, matching the already-FD
    // hw_protocol_id, so this must pass the new guard.
    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_BAUDRATE, 5_000_000).await;

    promote_via_update_param(&mut client, cll_handle).await;

    server.shutdown().await;
}

/// ADR-158 correction (Codex review, PR #30, round 4): before this fix,
/// `resolve_send_recv_tx` normalized `FD_CAN_PS` down to plain `CAN` for TX
/// size validation, so no FD-sized (>8 data byte) payload could ever be
/// sent, no matter how the link staged `CP_CANFDTxMaxDataLength`. An
/// FD-connected link with `TX_DL = 64` sending exactly 64 data bytes must
/// now succeed, the recorded frame must carry the full unpadded 64 bytes
/// (68 with the 4-byte header, no padding needed since 64 is itself a valid
/// SAE J2534-2 Table 91 length), and `TX_FD_CAN_FORMAT`/`TX_FD_CAN_BRS`
/// must both be set (a nonzero `CP_CANFDBaudrate` requests the data-phase
/// bit-rate switch).
#[tokio::test]
#[serial]
async fn fd_can_connected_link_sends_a_full_size_fd_payload() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let payload = vec![0x11u8; 64];
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
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("a 64-byte payload on a TX_DL=64 FD link should be accepted, not rejected");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.len(),
        4 + 64,
        "64-byte FD payload needs no padding -- it is itself a valid Table 91 length"
    );
    assert_eq!(&written[4..], payload.as_slice());
    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_FORMAT,
        0,
        "an FD-connected link's TX must carry TX_FD_CAN_FORMAT (SAE J2534-2 21.4.4)"
    );
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_BRS,
        0,
        "a nonzero staged CP_CANFDBaudrate requests the data-phase bit-rate switch"
    );

    server.shutdown().await;
}

/// Test-coverage-gap fix (edge-case-hunter, PR #30 round 4): `resolve_send_recv_tx`
/// has three call sites in `rpc_primitive.rs`, but only the `CoptSendrecv` one
/// (the test above) had FD-specific coverage. Round 4 traced by hand that
/// `CoptStartcomm`'s optional pre-message call site threads the same
/// `link.hw_protocol_id`/bound-`params` pairing as the tested `CoptSendrecv`
/// site, protected by the same `connect_generation` guard (ADR-086) -- this
/// test confirms that traced-by-hand reasoning with a real end-to-end run
/// rather than a code inspection. Same FD-connected link setup as the
/// `CoptSendrecv` test above (`CP_CANFDTxMaxDataLength=64`,
/// `CP_CANFDBaudrate=2_000_000`): a 64-byte optional `CoptStartcomm` message
/// must be accepted unpadded, and the written TxFlags must carry both
/// `TX_FD_CAN_FORMAT` and `TX_FD_CAN_BRS`.
#[tokio::test]
#[serial]
async fn fd_can_connected_link_sends_a_full_size_fd_startcomm_optional_message() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let payload = vec![0x33u8; 64];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: payload.clone(),
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
            "a 64-byte optional CoptStartcomm message on a TX_DL=64 FD link should be accepted, \
             not rejected",
        );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.len(),
        4 + 64,
        "64-byte FD optional message needs no padding -- it is itself a valid Table 91 length"
    );
    assert_eq!(&written[4..], payload.as_slice());
    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_FORMAT,
        0,
        "an FD-connected link's TX must carry TX_FD_CAN_FORMAT (SAE J2534-2 21.4.4)"
    );
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_BRS,
        0,
        "a nonzero staged CP_CANFDBaudrate requests the data-phase bit-rate switch"
    );

    server.shutdown().await;
}

/// Test-coverage-gap fix (edge-case-hunter, PR #30 round 4): the third
/// `resolve_send_recv_tx` call site, `CoptStopcomm`'s final message, traced
/// by hand to thread the same
/// `link.hw_protocol_id`/bound-`params` pairing (and the same
/// `connect_generation` guard, ADR-086) as the tested `CoptSendrecv` site --
/// confirmed here with a real run. Same FD-connected link setup as the
/// `CoptSendrecv` test above, comm started first via a plain `CoptStartcomm`:
/// a 64-byte final `CoptStopcomm` message must be accepted unpadded, and the
/// written TxFlags must carry both `TX_FD_CAN_FORMAT` and `TX_FD_CAN_BRS`.
#[tokio::test]
#[serial]
async fn fd_can_connected_link_sends_a_full_size_fd_stopcomm_final_message() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
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

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "a plain (empty cop_data) CoptStartcomm must not transmit anything"
    );

    let payload = vec![0x44u8; 64];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: payload.clone(),
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
            "a 64-byte final CoptStopcomm message on a TX_DL=64 FD link should be accepted, not \
             rejected",
        );

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.len(),
        4 + 64,
        "64-byte FD final message needs no padding -- it is itself a valid Table 91 length"
    );
    assert_eq!(&written[4..], payload.as_slice());
    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_FORMAT,
        0,
        "an FD-connected link's TX must carry TX_FD_CAN_FORMAT (SAE J2534-2 21.4.4)"
    );
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_BRS,
        0,
        "a nonzero staged CP_CANFDBaudrate requests the data-phase bit-rate switch"
    );

    drop(events);
    server.shutdown().await;
}

/// Companion to the test above: a payload exceeding the link's staged
/// `CP_CANFDTxMaxDataLength` is still rejected -- the fix widens the range
/// to match the link's live TX_DL, it does not remove the cap entirely.
#[tokio::test]
#[serial]
async fn fd_can_connected_link_rejects_a_payload_exceeding_staged_tx_dl() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 32),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x22u8; 33],
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
        .expect_err("a 33-byte payload on a TX_DL=32 FD link must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// A payload whose length isn't itself a Table 91 DLC-encoded length must be
/// padded up to the nearest one with `CP_CanFillerByte`, not rejected --
/// ISO 22900-2's `CP_CANFDTxMaxDataLength` NOTE 5 makes this the D-PDU
/// API's duty (ADR-158 correction, round 4).
#[tokio::test]
#[serial]
async fn fd_can_connected_link_pads_a_non_dlc_aligned_payload() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
            (CP_CAN_FILLER_BYTE, 0xAA),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let payload = vec![0x33u8; 40];
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
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect("a 40-byte payload should be padded up to 48, not rejected");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.len(),
        4 + 48,
        "40 data bytes must pad up to the next Table 91 length (48), not 64"
    );
    assert_eq!(&written[4..44], payload.as_slice());
    assert_eq!(
        &written[44..],
        [0xAAu8; 8].as_slice(),
        "the padding bytes must be CP_CanFillerByte"
    );

    server.shutdown().await;
}

/// Locks in the `.max(8)` fallback in `fd_can_tx_message_size_range`: an FD
/// link connected via `CP_CANFDBaudrate` alone (no explicit `TX_DL`, ISO
/// 22900-2's own default for an unset TX_DL is `8`) still enforces Classic
/// CAN's `4..=12` range rather than rejecting every payload outright, but
/// still tags FD-format frames as such.
#[tokio::test]
#[serial]
async fn fd_can_baudrate_only_link_falls_back_to_classic_size_range_but_stays_fd_tagged() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x44u8; 9],
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
        .expect_err("9 data bytes exceed the .max(8) fallback range on a TX_DL-unset FD link");
    assert_eq!(status.code(), Code::InvalidArgument);

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x44u8; 8],
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
        .expect("8 data bytes fit the .max(8) fallback range");

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_FORMAT,
        0,
        "still FD-connected (CP_CANFDBaudrate alone triggers FD mode), so still FD-tagged"
    );

    server.shutdown().await;
}

/// The TX-size cap tracks a live `CoptUpdateparam` promotion within FD mode
/// (not a value frozen at connect time): a link connected with `TX_DL = 12`
/// that later promotes `TX_DL = 64` (still FD -- round-3's guard permits
/// same-mode value changes) must accept a 64-byte payload afterward.
#[tokio::test]
#[serial]
async fn fd_can_tx_size_cap_tracks_a_live_coptupdateparam_promotion() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 12),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    set_com_param_unum32(&mut client, cll_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;
    promote_via_update_param(&mut client, cll_handle).await;

    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x55u8; 64],
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
        .expect("a 64-byte payload must be accepted after promoting TX_DL from 12 to 64");

    server.shutdown().await;
}

/// `resolve_tester_present`'s new FD-flag logic (round 4) had zero coverage
/// until an `edge-case-hunter` verification pass on this same correction
/// flagged it -- `CoptStartcomm` mode 0 (`CP_TesterPresentSendType = 0`)
/// arms and sends its first periodic tester-present frame as soon as
/// `CoptStartcomm` completes (mirrors `tester_present_reqrsp.rs`'s
/// `create_routed_cll` helper's own doc comment), so this needs no timer
/// wait beyond the usual write-visibility poll.
#[tokio::test]
#[serial]
async fn fd_can_tester_present_on_an_fd_connected_link_carries_fd_flags() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
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
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 0).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
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
    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_FORMAT,
        0,
        "an FD-connected link's tester-present frame must carry TX_FD_CAN_FORMAT too"
    );
    assert_ne!(
        tx_flags & j2534_0404::TX_FD_CAN_BRS,
        0,
        "the same nonzero staged CP_CANFDBaudrate applies to tester-present frames"
    );

    server.shutdown().await;
}

/// ADR-215 Decision item 3: an FD-connected `FD_CAN_PS` link's resolved
/// tester-present message is padded to the next DLC-legal length with
/// `CP_CanFillerByte`, mirroring `fd_can_connected_link_pads_a_non_dlc_aligned_payload`'s
/// identical `CoptSendrecv` coverage above -- a 9-byte payload (ISO
/// 22900-2-legal under the 12-byte `ParamMaxLen` cap, ADR-215 Decision item 1)
/// pads up to 12, not the link's full 64-byte staged `CP_CANFDTxMaxDataLength`.
#[tokio::test]
#[serial]
async fn fd_can_tester_present_pads_a_non_dlc_aligned_payload() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
            (CP_CAN_FILLER_BYTE, 0xAA),
        ],
    )
    .await;
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let payload = vec![0x33u8; 9];
    set_com_param_bytes(
        &mut client,
        cll_handle,
        CP_TESTER_PRESENT_MESSAGE,
        payload.clone(),
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_HANDLING, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_SEND_TYPE, 0).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME, 5_000_000).await;
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
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written.len(),
        4 + 12,
        "9 data bytes must pad up to the next Table 91 length (12), not 64"
    );
    assert_eq!(&written[4..13], payload.as_slice());
    assert_eq!(
        &written[13..],
        [0xAAu8; 3].as_slice(),
        "the padding bytes must be CP_CanFillerByte"
    );

    server.shutdown().await;
}

/// Locks in the `SetComParam`-time guard `fd_can_padded_data_len`'s
/// `.expect()` depends on never being violated (`edge-case-hunter`
/// verification pass on the round-4 correction) -- only the standalone
/// validator `is_valid_canfd_tx_max_data_length` was unit-tested before
/// this, not the RPC-level rejection itself.
#[tokio::test]
#[serial]
async fn setcomparam_rejects_an_out_of_range_canfd_tx_max_data_length() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, &[(3, "HI"), (11, "LOW")]),
        1,
    )
    .await
    .expect("create_com_logical_link should succeed");

    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    CP_CANFD_TX_MAX_DATA_LENGTH,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(40)),
            }),
        })
        .await
        .expect_err(
            "40 is not one of SAE J2534-2 Table 90/97's documented CP_CANFDTxMaxDataLength \
             encodings and must be rejected",
        );
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// A directly-named `PROTOCOL_FD_CAN_PS` protocol id is rejected with
/// `invalid_argument` at `CreateComLogicalLink` time -- never silently
/// normalized to a plain CAN connect (ADR-158, mirroring
/// `pin_selection.rs`'s/`additional_channels.rs`'s equivalent direct-`_PS`/
/// `_CHx`-naming rejection tests).
#[tokio::test]
#[serial]
async fn direct_fd_can_protocol_id_naming_is_rejected() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let status = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol(j2534_0404::PROTOCOL_FD_CAN_PS),
        1,
    )
    .await
    .expect_err("a directly-named FD_CAN_PS protocol id must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejection must happen at CreateComLogicalLink, before any connect is attempted"
    );

    server.shutdown().await;
}

/// ADR-158 Corrections item 3: `BIT_SAMPLE_POINT` (FD-read-only per clause
/// 21.3.2.5.1) can legitimately land in Active on an FD_CAN_PS link via
/// ordinary `SetComParam` + `ConnectComLogicalLink` (Active is the promoted,
/// `GetComParam`-visible ComParam set, ADR-067/068 -- it is not filtered for
/// FD-validity). It must still never reach a native `SET_CONFIG` call.
///
/// This test exercises two DIFFERENT protections on two different paths, not
/// one joint mechanism (correction, edge-case-hunter re-verification pass:
/// an earlier version of this comment claimed both paths pinned both
/// protections jointly -- disproven by temporarily reverting the raw-id
/// contract fix alone and confirming this test still passed):
/// - The `ParamBinding::Temp` apply+revert bracket
///   (`apply_params_to_hardware`/`revert_hardware_to_live_active`) is
///   protected ONLY by ADR-110's unconditional `PDU_PC_BUSTYPE`-class strip
///   before any hardware push -- both `BIT_SAMPLE_POINT` and
///   `SYNC_JUMP_WIDTH` are `BUSTYPE_UNUM32`, so they never even reach
///   `to_j2534_config_id`'s FD suppression at this bracket's call sites.
///   Reverting the raw-id contract fix alone does NOT make this bracket
///   forward the param -- this path's coverage here is really an ADR-110
///   regression test, not an ADR-158 one.
/// - `set_can_phys_req_id_and_promote` below drives a `CoptUpdateparam`
///   promotion via `apply_bustype_lock`, which does NOT strip
///   `BUSTYPE_UNUM32` keys -- this is the one path in this test where the
///   raw-id contract fix (Corrections item 1) is actually load-bearing:
///   `apply_params_to_hardware_locked` must receive the raw hw id for
///   `to_j2534_config_id`'s FD suppression to fire here at all.
#[tokio::test]
#[serial]
async fn fd_can_ps_never_forwards_bit_sample_point_across_temp_bracket() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    // SetComParam(BIT_SAMPLE_POINT, ...) then ConnectComLogicalLink as FD:
    // BIT_SAMPLE_POINT promotes into Active at connect time
    // (`link.active = working_snapshot`) even though it is FD-invalid on
    // this link and never forwarded to hardware itself
    // (`to_j2534_config_id`'s FD suppression skips it at connect time too).
    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
            (j2534_0404::BIT_SAMPLE_POINT, 80),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: this link must actually be FD_CAN_PS for the suppression under test to be \
         exercised at all"
    );

    // Also exercises the second Active-population path Finding 2 identified
    // (a `CoptUpdateparam` promotion via `apply_bustype_lock`, and its own
    // `apply_params_to_hardware_locked` push) -- required anyway on a
    // CAN-family link before `CoptSendrecv` can send at all.
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // A Temp-binding CoptSendrecv exercises both `apply_params_to_hardware`
    // call sites in this bracket (the Temp apply itself, then
    // `revert_hardware_to_live_active`'s revert to the live Active set,
    // which carries BIT_SAMPLE_POINT=80) -- protected here by ADR-110's
    // unconditional BUSTYPE strip, not by the raw-id contract fix (see this
    // test's own doc comment above).
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

    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        !log.contains(&j2534_0404::BIT_SAMPLE_POINT),
        "BIT_SAMPLE_POINT must never appear in a native SET_CONFIG call on an FD_CAN_PS link, \
         across connect, a Temp-binding apply+revert bracket, and a CoptUpdateparam promotion; \
         full SET_CONFIG parameter log was {log:#x?}"
    );

    drop(events);
    server.shutdown().await;
}

// ── ADR-158 Correction (Codex review, PR #30): `ChannelKey` widened to a
// 4-tuple, appending the link's effective CAN FD data-phase rate ──────────
//
// Two `FD_CAN_PS` CLLs with matching arbitration `baud_rate`/`pin_select`
// but different `CP_CANFDBaudrate` values used to collapse onto the same
// `ChannelKey` and silently share one physical channel: only the creator's
// data-phase rate was ever applied to hardware, and a joining CLL's own
// rate was never applied at all. Widening `ChannelKey` to also carry the
// effective data-phase rate (`CP_CANFDBaudrate` if nonzero, else
// `DATA_RATE`; `0` for every non-FD link) closes that gap the same way
// ADR-156 Decision 2 closed the analogous `pin_select` gap.

/// Two FD_CAN_PS CLLs whose raw `CP_CANFDBaudrate` ComParam differs (`0`,
/// the "use DATA_RATE" sentinel, vs. an explicit value equal to DATA_RATE)
/// but whose EFFECTIVE data-phase rate is identical must still share one
/// physical channel -- proving `ChannelKey` keys on the effective rate
/// (`rpc_link.rs`'s `fd_data_phase_rate` local), not the raw ComParam
/// value, which would wrongly split these into two channels.
#[tokio::test]
#[serial]
async fn same_effective_data_phase_rate_shares_one_physical_channel_despite_differing_raw_comparam()
{
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let _cll_a = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 0),
        ],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_FD_CAN_DATA_PHASE_RATE),
        500_000,
        "CP_CANFDBaudrate == 0 should fall back to DATA_RATE for the effective data-phase rate"
    );

    let _cll_b = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 500_000),
        ],
    )
    .await;

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "a second FD_CAN_PS CLL whose raw CP_CANFDBaudrate (500_000, explicit) differs from the \
         first's (0, the DATA_RATE-fallback sentinel) but whose EFFECTIVE data-phase rate is \
         identical (both 500_000) must join the first CLL's already-open physical channel, not \
         open a second one -- proving ChannelKey keys on the effective rate, not the raw \
         ComParam value"
    );

    server.shutdown().await;
}

/// Two FD_CAN_PS CLLs with genuinely different effective data-phase rates
/// (same arbitration baud rate and, since neither supplies `dlc_pin_data`,
/// the same default packed pins) must NOT share a physical channel: the
/// widened `ChannelKey` gives them distinct keys, so the second CLL opens
/// its own new physical channel and applies its OWN data-phase rate to
/// hardware -- the bug this fix closes (before it, only the first CLL's
/// rate was ever applied, and the second silently ran at a rate it never
/// actually configured).
#[tokio::test]
#[serial]
async fn differing_effective_data_phase_rate_opens_a_second_physical_channel() {
    const SECOND_CHANNEL_ID: u32 = 2;

    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let _cll_a = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_FD_CAN_DATA_PHASE_RATE),
        2_000_000
    );

    let _cll_b = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 5_000_000),
        ],
    )
    .await;

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "differing effective data-phase rates must open a second, distinct physical channel \
         even though protocol, arbitration baud rate, and (default) pins are identical -- the \
         widened ChannelKey's fourth element keeps them separated"
    );
    assert_eq!(
        server.backdoor.channel_protocol_id(SECOND_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS
    );
    assert_eq!(
        server
            .backdoor
            .config_value(SECOND_CHANNEL_ID, j2534_0404::CONFIG_FD_CAN_DATA_PHASE_RATE),
        5_000_000,
        "the second (joining, per the pre-fix ChannelKey) CLL's own data-phase rate must now \
         actually be applied to hardware on its own physical channel -- before this fix, a \
         joiner's rate was never applied at all"
    );

    server.shutdown().await;
}

// ── ADR-158 Correction (Codex review, PR #30, declined): CAN FD data-phase
// rate deliberately excluded from the physical-lock fallback comparison ────
//
// Codex suggested widening `find_physical_lock_holder`/`same_physical_resource`
// (`service.rs`) to also compare the CAN FD effective data-phase rate,
// mirroring how `pin_select` was threaded through for ADR-157 Bug D. This
// was investigated and declined as a false positive: `CP_CANFDBaudrate` (like
// `CP_Baudrate`) is itself `BUSTYPE_UNUM32`-class configuration --
// exactly the class `LOCK_PHYSICAL_COM_PARAMS` exists to protect -- so
// including it would let a CLL bypass another CLL's lock simply by staging a
// different value for the very parameter class the lock protects. See
// `same_physical_resource`'s doc comment (`service.rs`) for the full
// rationale. The two tests below pin the correct (declined-finding) behavior
// down both directions: same pins/different rate must still conflict; genuinely
// different pins must not.

/// Two FD_CAN_PS links with the SAME EXPLICIT `dlc_pin_data` (avoiding the
/// separate, already-tracked ADR-158 `None`-vs-synthesized-default
/// `pin_select` residual an unqualified/unqualified pair would otherwise
/// also exercise -- this test isolates the rate comparison alone), differing
/// only in their effective CAN FD data-phase rate: `cll_a` connects, locks
/// `LOCK_PHYSICAL_COM_PARAMS`, and `cll_b`'s subsequent connect attempt at a
/// DIFFERENT data-phase rate on those same pins must be rejected as a lock
/// conflict -- proving the rate is correctly EXCLUDED from the fallback
/// comparison (same pins is what makes this a real physical conflict, not
/// the differing rate, which must not be read as "a different resource").
#[tokio::test]
#[serial]
async fn fd_can_lock_conflict_blocks_a_same_pins_different_rate_connect_attempt() {
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;
    const PINS: &[(u32, &str)] = &[(3, "HI"), (11, "LOW")];

    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_a_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, PINS),
        1,
    )
    .await
    .expect("create_com_logical_link should succeed for cll_a's pin selection");
    set_com_param_unum32(&mut client, cll_a_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;
    set_com_param_unum32(&mut client, cll_a_handle, CP_CANFD_BAUDRATE, 2_000_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a_handle),
        })
        .await
        .expect("cll_a's connect_com_logical_link should succeed");
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: cll_a must actually be FD_CAN_PS for this lock-conflict scenario to apply"
    );

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a_handle),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect("lock_resource(LOCK_PHYSICAL_COM_PARAMS) should succeed on cll_a");

    let cll_b_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, PINS),
        2,
    )
    .await
    .expect("create_com_logical_link should succeed for cll_b's (identical) pin selection");
    set_com_param_unum32(&mut client, cll_b_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_b_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;
    // Different effective data-phase rate from cll_a's (5_000_000 vs
    // 2_000_000) -- must NOT be read as "a different physical resource".
    set_com_param_unum32(&mut client, cll_b_handle, CP_CANFD_BAUDRATE, 5_000_000).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b_handle),
        })
        .await
        .expect_err(
            "cll_b's connect must be rejected: same explicit pins as cll_a's locked FD_CAN_PS \
             link, differing only in data-phase rate -- rate is BUSTYPE-class configuration the \
             lock protects, not physical identity, so it must not carve out a bypass",
        );
    assert_eq!(status.code(), Code::ResourceExhausted);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrRscLockedByOtherCll as i32
    );
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "the rejected connect must not have opened a second physical channel"
    );

    server.shutdown().await;
}

/// Two FD_CAN_PS links with genuinely DIFFERENT explicit pin selections
/// (and, incidentally, different data-phase rates too): `cll_a` connects and
/// locks `LOCK_PHYSICAL_COM_PARAMS`, and `cll_b`'s connect on different pins
/// must NOT be blocked -- `pin_select` already discriminates the
/// genuinely-different-physical-bus case, so there is no lock conflict here
/// regardless of the differing rate.
#[tokio::test]
#[serial]
async fn fd_can_lock_does_not_block_a_different_pins_different_rate_connect_attempt() {
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;

    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_a_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, &[(3, "HI"), (11, "LOW")]),
        1,
    )
    .await
    .expect("create_com_logical_link should succeed for cll_a's pin selection");
    set_com_param_unum32(&mut client, cll_a_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_a_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;
    set_com_param_unum32(&mut client, cll_a_handle, CP_CANFD_BAUDRATE, 2_000_000).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_a_handle),
        })
        .await
        .expect("cll_a's connect_com_logical_link should succeed");
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: cll_a must actually be FD_CAN_PS for this scenario to apply"
    );

    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a_handle),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect("lock_resource(LOCK_PHYSICAL_COM_PARAMS) should succeed on cll_a");

    // cll_b: DIFFERENT pins, DIFFERENT data-phase rate -- must connect
    // successfully, opening its own distinct physical channel, despite
    // cll_a's lock.
    let cll_b_handle = try_create_cll_with_resource(
        &mut client,
        1,
        resource_with_protocol_id_and_pins(j2534_0404::CAN, &[(1, "HI"), (9, "LOW")]),
        2,
    )
    .await
    .expect("create_com_logical_link should succeed for cll_b's differing pin selection");
    set_com_param_unum32(&mut client, cll_b_handle, j2534_0404::DATA_RATE, 500_000).await;
    set_com_param_unum32(&mut client, cll_b_handle, CP_CANFD_TX_MAX_DATA_LENGTH, 64).await;
    set_com_param_unum32(&mut client, cll_b_handle, CP_CANFD_BAUDRATE, 5_000_000).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b_handle),
        })
        .await
        .expect(
            "cll_b's connect must succeed: genuinely different pins from cll_a's locked \
             FD_CAN_PS link open a distinct physical channel, not a lock conflict, regardless \
             of the differing data-phase rate",
        );

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "cll_b's differing pin selection must open a second, distinct physical channel"
    );

    server.shutdown().await;
}

/// ADR-186 regression test: `install_client_message_filters` must OR
/// `TX_FD_CAN_FORMAT` into a client-installed filter's `TxFlags` for any
/// FD-connected link (mirroring `ioctl_start_repeat_message`'s own round-15
/// precedent, ADR-165 PR #42) -- SAE J2534-2 21.4.4 lets a device cap a
/// `PASSTHRU_MSG`'s DataSize at 12 bytes when the flag is unset, so a
/// client-supplied filter mask/pattern template on an FD-connected link
/// needs it set for template VALIDITY, even though clause 21.2.2(g) requires
/// FD-capable-channel filtering itself to ignore CAN message format. Raw CAN
/// always connects `CAN_ID_BOTH` (ADR-065), so a single client filter
/// installs twice -- once per `TxFlags`/ID-width variant -- both of which
/// must carry `TX_FD_CAN_FORMAT`. `TX_FD_CAN_BRS` must NOT be set (Table 93:
/// a bit-timing property, not a format discriminator, with no analogous
/// DataSize-validity coupling on a never-transmitted template).
#[tokio::test]
#[serial]
async fn client_filter_on_an_fd_connected_link_carries_tx_fd_can_format() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[
            (j2534_0404::DATA_RATE, 500_000),
            (CP_CANFD_TX_MAX_DATA_LENGTH, 64),
            (CP_CANFD_BAUDRATE, 2_000_000),
        ],
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: staged FD ComParams should have substituted the native connect id to \
         FD_CAN_PS (ADR-158) -- otherwise this test would not actually be exercising an \
         FD-connected link"
    );

    let filter_count_before = server.backdoor.filter_count(MOCK_CHANNEL_ID);
    install_client_pass_filter(&mut client, cll_handle).await;
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID) - filter_count_before,
        2,
        "a single client filter on a CAN_ID_BOTH-connected channel should install once per \
         TxFlags/ID-width variant (TxFlags 0 and TX_EXTENDED_ID)"
    );

    for index in filter_count_before..filter_count_before + 2 {
        let tx_flags = server
            .backdoor
            .filter_pattern_tx_flags(MOCK_CHANNEL_ID, index);
        assert_ne!(
            tx_flags & j2534_0404::TX_FD_CAN_FORMAT,
            0,
            "an FD-connected link's client filter (index {index}) must carry TX_FD_CAN_FORMAT \
             (SAE J2534-2 21.4.4 template-DataSize validity): {tx_flags:#06x}"
        );
        assert_eq!(
            tx_flags & j2534_0404::TX_FD_CAN_BRS,
            0,
            "the client filter (index {index}) must NOT carry TX_FD_CAN_BRS -- BRS is a \
             bit-timing property (Table 93), not a format discriminator, with no \
             template-validity coupling: {tx_flags:#06x}"
        );
    }

    server.shutdown().await;
}

/// Companion assertion (ADR-186): a CLASSIC (non-FD) CAN link's client
/// filter TxFlags must be unaffected by this fix -- still exactly `0`/
/// `TX_EXTENDED_ID` per `can_filter_tx_flags`'s pre-existing logic, with no
/// `TX_FD_CAN_FORMAT` bit added.
#[tokio::test]
#[serial]
async fn client_filter_on_a_classic_can_link_is_unaffected_by_the_fd_can_format_fix() {
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        1,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;
    assert_ne!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_FD_CAN_PS,
        "sanity: this link must NOT be FD-substituted -- no FD ComParams were staged"
    );

    let filter_count_before = server.backdoor.filter_count(MOCK_CHANNEL_ID);
    install_client_pass_filter(&mut client, cll_handle).await;
    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID) - filter_count_before,
        2,
        "a single client filter on a CAN_ID_BOTH-connected channel should install once per \
         TxFlags/ID-width variant (TxFlags 0 and TX_EXTENDED_ID)"
    );

    let first_tx_flags = server
        .backdoor
        .filter_pattern_tx_flags(MOCK_CHANNEL_ID, filter_count_before);
    let second_tx_flags = server
        .backdoor
        .filter_pattern_tx_flags(MOCK_CHANNEL_ID, filter_count_before + 1);
    assert_eq!(
        first_tx_flags, 0,
        "a classic CAN link's client filter TxFlags must be unaffected by the FD fix: \
         {first_tx_flags:#06x}"
    );
    assert_eq!(
        second_tx_flags,
        j2534_0404::TX_EXTENDED_ID,
        "a classic CAN link's second (29-bit) client filter TxFlags must be unaffected by the \
         FD fix: {second_tx_flags:#06x}"
    );

    server.shutdown().await;
}

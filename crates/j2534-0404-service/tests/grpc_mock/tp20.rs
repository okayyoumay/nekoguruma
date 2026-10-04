//! End-to-end coverage for SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188,
//! Phase 7 Stage 7a): the standalone `TP2_0_PS` resource row's mandatory
//! internal `SET_CONFIG(CONFIG_J1962_PINS)`, the closed single pin-pair set
//! (clause 19.2.2: pins 6/14, and no other pair), the five minted
//! `PARAM_TP20_*` ComParams round-tripping via `SetComParam`/`GetComParam`,
//! the connection-request lifecycle `CoptStartcomm`/`CoptStopcomm` drive
//! (ADR-188 Decision item 2), connection-rejection when all four native
//! slots are full, TX framing (the established TX-ID prepended to a
//! `CoptSendrecv` payload), RX-ID-matched routing across two sibling CLLs
//! sharing one physical channel, TX-ID-matched routing of a `CP_Loopback`-
//! enabled write's own echo back to its originating connection, and the
//! CLL-per-connection sharing model
//! itself (two CLLs independently establishing/tearing down their own
//! connection on one shared physical channel). Mirrors `j1939.rs`'s/
//! `j1708.rs`'s structure for the harness/helper conventions; a few small
//! per-file-local helpers are duplicated the same way those files duplicate
//! their own (this codebase's existing convention for this shape of
//! helper).

use serial_test::serial;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveCtrlData, ComPrimitiveHandle, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DataItem, DestroyComLogicalLinkRequest,
    DisconnectComLogicalLinkRequest, ExpectedResponseData, GetObjectIdRequest, GetStatusRequest,
    IoBytearray, LockResourceRequest, ModuleHandle, ObjectType, PduComLogicalLinkStatus,
    PduComPrimitiveStatus, PduErrorEvent, PinData, ResourceData, StartComPrimitiveRequest,
    UnlockResourceRequest, create_com_logical_link_request, data_item, event_item,
    get_status_request, io_ctl_request, resource_data, status_response, subscribe_event_request,
};

use crate::harness::*;

/// Resource id of the SAE J2534-2 clause 19 TP2.0 Protocol row --
/// `resources.rs` row 0x025F, `protocol: ChannelProtocol::TP2_0_PS`,
/// `hw_protocol_override: None` (the `_PS` id IS the identity, the same
/// shape UART Echo Byte/Honda DIAG-H/J1708/J1939 already use).
const TP2_0_RESOURCE_ID: u32 = 0x025F;

/// D-PDU `CP_TP20ChannelSetupCanId` ComParam ID
/// (`service_params::PARAM_TP20_CHANNEL_SETUP_CAN_ID`, 0x80C9): packs into
/// Table 78 bytes 0-3 of `IOCTL_REQUEST_CONNECTION`'s request.
const CP_TP20_CHANNEL_SETUP_CAN_ID: u32 = 0x80C9;

/// D-PDU `CP_TP20DestinationAddress` ComParam ID
/// (`service_params::PARAM_TP20_DESTINATION_ADDRESS`, 0x80CA): packs into
/// Table 78 byte 4.
const CP_TP20_DESTINATION_ADDRESS: u32 = 0x80CA;

/// D-PDU `CP_TP20TxIdProposal` ComParam ID
/// (`service_params::PARAM_TP20_TX_ID_PROPOSAL`, 0x80CB): packs into Table
/// 78 bytes 6-7. NOT overwritten with the established TX-ID once
/// `CoptStartcomm` succeeds (Codex review PR #97 fix, ADR-188): an earlier
/// version wrote the assigned TX-ID back into this same client-writable
/// ComParam for `tx_header::build_tx_message` to read -- since a client can
/// `SetComParam` this ComParam to any value before `CoptStartcomm` ever
/// runs, that write-back made the ComParam ambiguous between "client
/// proposal" and "service-assigned result." TX framing now reads the
/// established TX-ID from `LogicalLinkState::tp20_connection` directly, so
/// this ComParam always reflects only whatever a client last staged on it.
const CP_TP20_TX_ID_PROPOSAL: u32 = 0x80CB;

/// D-PDU `CP_TP20RxIdProposal` ComParam ID
/// (`service_params::PARAM_TP20_RX_ID_PROPOSAL`, 0x80CC): packs into Table
/// 78 bytes 8-9; also the key this CLL's routing-map entry is registered
/// under.
const CP_TP20_RX_ID_PROPOSAL: u32 = 0x80CC;

/// D-PDU `CP_TP20ApplicationType` ComParam ID
/// (`service_params::PARAM_TP20_APPLICATION_TYPE`, 0x80CD): packs into
/// Table 78 byte 10.
const CP_TP20_APPLICATION_TYPE: u32 = 0x80CD;

/// D-PDU `CP_TP20PassiveIdentifier` ComParam ID
/// (`service_params::PARAM_TP20_PASSIVE_IDENTIFIER`, 0x80CE, ADR-190/Phase 7
/// Stage 7b): applied directly via native `SET_CONFIG(CONFIG_TP2_0_
/// IDENTIFER)` when arming the passive listener.
const CP_TP20_PASSIVE_IDENTIFIER: u32 = 0x80CE;

/// D-PDU `CP_TP20PassiveRxId` ComParam ID
/// (`service_params::PARAM_TP20_PASSIVE_RX_ID`, 0x80CF, ADR-190/Phase 7
/// Stage 7b): applied directly via native `SET_CONFIG(CONFIG_TP2_0_
/// RXIDPASSIVE)` when arming the passive listener -- also the key the
/// passive slot's own persistent routing entry is registered under.
const CP_TP20_PASSIVE_RX_ID: u32 = 0x80CF;

/// The one pin pair clause 19.2.2 documents (J1962 pins 6/14, CAN_H/CAN_L).
const TP2_0_PINS: &[(u32, &str)] = &[(6, "HI"), (14, "LOW")];

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// every TP2.0 resource row requires this opt-in (`names.rs`'s dedicated arm
/// in `resolve_pin_selection`, mirroring every other J2534-2 protocol's own
/// opt-in gate).
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

/// Builds a `RscData` resource selecting `protocol_id` via the raw
/// hardware-protocol-id route, with the given typed `(pin_number,
/// pin_type_name)` pairs as `dlc_pin_data` -- same shape as `j1939.rs`'s/
/// `j1708.rs`'s own `resource_with_protocol_id_and_pins`.
fn resource_with_protocol_id_and_pins(protocol_id: u32, pins: &[(u32, &str)]) -> ResourceData {
    ResourceData {
        dlc_pin_data: pins
            .iter()
            .map(|&(number, type_name)| PinData {
                dlc_pin_number: number,
                dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                    type_name.to_string(),
                )),
            })
            .collect(),
        bus_type: None,
        protocol: Some(resource_data::Protocol::ProtocolId(protocol_id)),
    }
}

/// Creates (but does not connect) a TP2.0 CLL for `resource_id` with the
/// given `dlc_pin_data`.
async fn create_tp20_cll(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource_id: u32,
    pins: &[(u32, &str)],
    _cll_tag: u64,
) -> ComLogicalLinkHandle {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(resource_id, pins),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

/// Creates and connects a TP2.0 CLL, staging all five mandatory
/// `PARAM_TP20_*` ComParams before `ConnectComLogicalLink` (so they are
/// already part of the Active snapshot an ordinary, non-`temp_param_update`
/// `CoptStartcomm` binds from -- ADR-067) with `rx_id_proposal` as the one
/// varying field (the per-CLL identity a sibling CLL sharing the same
/// physical channel must differ on). Thin wrapper over
/// `create_and_connect_tp20_cll_for_protocol_id_and_pins` fixed to the `_PS`
/// resource id and its own closed pin pair.
async fn create_and_connect_tp20_cll(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    rx_id_proposal: u32,
    cll_tag: u64,
) -> ComLogicalLinkHandle {
    create_and_connect_tp20_cll_for_protocol_id_and_pins(
        client,
        TP2_0_RESOURCE_ID,
        TP2_0_PINS,
        rx_id_proposal,
        cll_tag,
    )
    .await
}

/// ADR-210: generalizes `create_and_connect_tp20_cll` to any directly-named
/// hardware protocol id (e.g. `PROTOCOL_TP2_0_CH1`, with empty `pins` -- a
/// `_CHx` id has no J1962 pin concept at all) instead of hardcoding
/// `TP2_0_RESOURCE_ID`/`TP2_0_PINS`, so `_CHx` connect tests can reuse the
/// identical five-ComParam staging/promote/drain sequence.
async fn create_and_connect_tp20_cll_for_protocol_id_and_pins(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    protocol_id: u32,
    pins: &[(u32, &str)],
    rx_id_proposal: u32,
    cll_tag: u64,
) -> ComLogicalLinkHandle {
    let cll_handle = create_tp20_cll(client, protocol_id, pins, cll_tag).await;

    set_com_param_unum32(
        client,
        cll_handle,
        CP_TP20_CHANNEL_SETUP_CAN_ID,
        0x0000_0700,
    )
    .await;
    set_com_param_unum32(client, cll_handle, CP_TP20_DESTINATION_ADDRESS, 0x10).await;
    set_com_param_unum32(client, cll_handle, CP_TP20_TX_ID_PROPOSAL, 0x0300).await;
    set_com_param_unum32(client, cll_handle, CP_TP20_RX_ID_PROPOSAL, rx_id_proposal).await;
    set_com_param_unum32(client, cll_handle, CP_TP20_APPLICATION_TYPE, 1).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // Only the FIRST CLL on a fresh physical channel gets its staged Working
    // ComParams promoted to Active automatically at connect time -- a
    // CLL joining an already-open shared channel leaves Active at its
    // type-default until an explicit `CoptUpdateparam`
    // (`rpc_link.rs::rpc_connect_com_logical_link`'s own "Joining CLLs:
    // Active stays default until the CLL issues CoptUpdateparam" comment).
    // `CoptStartcomm`'s ordinary (non-Temp) binding resolves against Active,
    // so every CLL in this file promotes explicitly, harmless for the first
    // CLL where Active already equals Working.
    promote_via_update_param(client, cll_handle).await;

    // Drain the CoptUpdateparam's own CllStatus/CopStatus event backlog
    // before returning -- every test below subscribes to this CLL's events
    // AFTER calling this helper and expects the stream to start clean; an
    // un-drained backlog would otherwise let a `wait_for_event` predicate
    // scoped only to "the next PduCopstFinished" spuriously match this
    // CoptUpdateparam's own completion instead of the real COP under test
    // (ADR-149's own "scope assertions to the entity under test" guidance,
    // applied here to a prior COP on the SAME entity rather than a
    // different one).
    loop {
        let response = client
            .get_event_item(vci_service_interface::GetEventItemRequest {
                handle: Some(
                    vci_service_interface::get_event_item_request::Handle::CllHandle(cll_handle),
                ),
            })
            .await
            .expect("get_event_item should succeed")
            .into_inner();
        if response.event_item.is_none() {
            break;
        }
    }

    cll_handle
}

/// Creates and connects a TP2.0 CLL staged for ADR-190/Phase 7 Stage 7b's
/// passive arm: `CP_TP20PassiveIdentifier`/`CP_TP20PassiveRxId` instead of
/// Stage 7a's five active-connection ComParams. Thin wrapper over
/// `create_and_connect_tp20_cll_passive_for_protocol_id_and_pins` fixed to
/// the `_PS` resource id and its own closed pin pair -- same generalization
/// relationship `create_and_connect_tp20_cll` has to
/// `create_and_connect_tp20_cll_for_protocol_id_and_pins` (ADR-210).
async fn create_and_connect_tp20_cll_passive(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    identifier: u32,
    rx_id_passive: u32,
    cll_tag: u64,
) -> ComLogicalLinkHandle {
    create_and_connect_tp20_cll_passive_for_protocol_id_and_pins(
        client,
        TP2_0_RESOURCE_ID,
        TP2_0_PINS,
        identifier,
        rx_id_passive,
        cll_tag,
    )
    .await
}

/// ADR-210: generalizes `create_and_connect_tp20_cll_passive` to any
/// directly-named hardware protocol id (e.g. `PROTOCOL_TP2_0_CH1`, with
/// empty `pins` -- a `_CHx` id has no J1962 pin concept at all) instead of
/// hardcoding `TP2_0_RESOURCE_ID`/`TP2_0_PINS`, mirroring
/// `create_and_connect_tp20_cll_for_protocol_id_and_pins`'s own
/// generalization of the active-arm helper, so `_CHx` passive-listener tests
/// can reuse the identical arm/promote/drain sequence.
async fn create_and_connect_tp20_cll_passive_for_protocol_id_and_pins(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    protocol_id: u32,
    pins: &[(u32, &str)],
    identifier: u32,
    rx_id_passive: u32,
    cll_tag: u64,
) -> ComLogicalLinkHandle {
    let cll_handle = create_tp20_cll(client, protocol_id, pins, cll_tag).await;

    set_com_param_unum32(client, cll_handle, CP_TP20_PASSIVE_IDENTIFIER, identifier).await;
    set_com_param_unum32(client, cll_handle, CP_TP20_PASSIVE_RX_ID, rx_id_passive).await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    promote_via_update_param(client, cll_handle).await;

    loop {
        let response = client
            .get_event_item(vci_service_interface::GetEventItemRequest {
                handle: Some(
                    vci_service_interface::get_event_item_request::Handle::CllHandle(cll_handle),
                ),
            })
            .await
            .expect("get_event_item should succeed")
            .into_inner();
        if response.event_item.is_none() {
            break;
        }
    }

    cll_handle
}

/// Issues `CoptStartcomm` with no optional message and no
/// `temp_param_update` (the ordinary Active-snapshot path, ADR-067) -- TP2.0
/// has no COP-borne initialization payload at all (ADR-188 Decision item 2
/// step 1), so `cop_data` is always empty here.
async fn start_comm(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: ComLogicalLinkHandle,
) -> Result<(), tonic::Status> {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .map(|_| ())
}

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)` -- same per-file local helper shape as
/// `j1939.rs`'s/`repeat_message.rs`'s own (not shared via `harness.rs`,
/// matching this codebase's existing convention). Needed by the
/// `PDU_IOCTL_SUSPEND_TX_QUEUE`/`RESUME_TX_QUEUE` regression test below.
async fn resolve_ioctl_id(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
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

/// Issues `IoCtl` against a `cll_handle` with an optional `input_data` and
/// `has_output` flag. Mirrors `j1939.rs`'s own `io_ctl_cll`.
async fn io_ctl_cll(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: ComLogicalLinkHandle,
    cmd_id: u32,
    input_data: Option<DataItem>,
    has_output: bool,
) -> Result<Option<DataItem>, tonic::Status> {
    client
        .io_ctl(vci_service_interface::IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data,
            has_output,
        })
        .await
        .map(|resp| resp.into_inner().output_data)
}

/// Polls `GetStatus(cop_handle)` until it reports `PduCopstExecuting`, or
/// panics after ~2s -- makes "this CoptStartcomm has actually begun
/// dispatching" a structural precondition rather than a timing guess.
/// Mirrors `j1939.rs`'s/`locks_and_param_classes.rs`'s identical
/// `wait_for_cop_executing` helper (not shared via `harness.rs`, matching
/// this codebase's existing per-file-duplication convention). Needed by the
/// `tp20_no_indication`-driven mid-wait-staleness regression test below.
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

/// ADR-188 §1 documents `install_pass_all_filter` as skipped for a TP2.0
/// channel ("clause 19's per-connection addressing is the RX model, not a
/// pass-all baseline"), the same exclusion clause 10 Analog Inputs already
/// gets (ADR-177) -- but the exclusion was never actually wired into
/// `rpc_link.rs::connect_new_physical_channel`'s filter gate, so every
/// TP2.0 physical-link connect silently installed one anyway (Codex review
/// finding, round 26, PR #97). This test connects a TP2.0 CLL and asserts
/// no filter was installed at connect time, before any `CoptStartcomm`
/// ever runs -- the connect-time step alone, independent of whether a
/// connection is ever actually requested.
#[tokio::test]
#[serial]
async fn physical_link_connect_never_installs_a_pass_all_filter() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        0,
        "connecting a TP2.0 physical link must never install a pass-all filter -- clause 19's \
         per-connection addressing is the RX model, not a pass-all baseline (ADR-188 §1)"
    );

    server.shutdown().await;
}

/// A second, independent site with the identical gap the previous test
/// covers (found by a mandatory `edge-case-hunter` pass on that fix, round
/// 26, PR #97): `PDU_IOCTL_CLEAR_MSG_FILTERS`'s own reinstall path
/// (`rpc_misc.rs`) unconditionally reinstalled a pass-all filter for any
/// non-ISO15765 channel, with no TP2.0 (or Analog Input) exclusion --
/// independent of whether connect time itself ever tried to install one.
/// Issuing `CLEAR_MSG_FILTERS` against an already-connected TP2.0 CLL must
/// leave the filter count at zero, not silently reinstall one.
#[tokio::test]
#[serial]
async fn clear_msg_filters_never_reinstalls_a_pass_all_filter_for_tp20() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    assert_eq!(server.backdoor.filter_count(MOCK_CHANNEL_ID), 0);

    client
        .io_ctl(vci_service_interface::IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                j2534_0404::CLEAR_MSG_FILTERS,
            )),
            input_data: None,
            has_output: false,
        })
        .await
        .expect("io_ctl(CLEAR_MSG_FILTERS) should succeed");

    assert_eq!(
        server.backdoor.filter_count(MOCK_CHANNEL_ID),
        0,
        "CLEAR_MSG_FILTERS against a TP2.0 channel must never reinstall a pass-all filter -- \
         clause 19's per-connection addressing is the RX model, not a pass-all baseline \
         (ADR-188 §1), and there was nothing to reinstall in the first place"
    );

    server.shutdown().await;
}

/// Item 1 (ADR-188 Decision item 2's key behavioral proof): staging all five
/// `PARAM_TP20_*` ComParams then `CoptStartcomm` completes successfully
/// (`PduCopstFinished`, not `PduCopstCancelled`), establishes a connection
/// (confirmed via the mock's own mock-assigned TX-ID reaching the wire on a
/// subsequent `CoptSendrecv`), and `CoptStopcomm`/`DisconnectComLogicalLink`
/// tear it down cleanly.
#[tokio::test]
#[serial]
async fn connection_establishes_and_tears_down_cleanly() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_TP2_0_PS
    );

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish (PduCopstFinished) once the connection establishes"
    );
    drop(events);

    // TX framing: the established TX-ID (the mock's own `rx_id | 0x1000_0000`
    // assignment, `j2534-0404-mock`'s `IOCTL_REQUEST_CONNECTION` handler)
    // must prefix the payload on the wire.
    send_data(&mut client, cll_handle, vec![0xAA, 0xBB, 0xCC], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written,
        vec![0x10, 0x00, 0x03, 0x21, 0xAA, 0xBB, 0xCC],
        "the established TX-ID (0x1000_0321) should prefix the CoptSendrecv payload"
    );

    // Teardown: CoptStopcomm best-effort tears down the native connection,
    // then Disconnect releases the physical channel (sole owner, ref_count
    // hits 0).
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
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");
    assert_eq!(server.backdoor.disconnect_count(), 1);

    server.shutdown().await;
}

/// Item 1b (Codex review fix, PR #97, round 13): two concurrent
/// `CoptStartcomm` RPCs on the SAME `cll_handle`. `comm_started` is only
/// ever set `true` at the very end of `handle_start_comm`'s overall
/// processing (well after a TP2.0 connection actually establishes), so both
/// RPCs can pass `rpc_primitive.rs`'s own pre-flight/TOCTOU-recheck
/// precondition and get queued as two separate `TxItem::StartComm` entries,
/// which this codebase's single-poll-task-per-physical-channel
/// serialization then runs one after the other. Before this fix, the SECOND
/// attempt to dispatch unconditionally overwrote the first attempt's own
/// just-established `Tp20Connection` state with a fresh `Requested`, then
/// issued a duplicate native `IOCTL_REQUEST_CONNECTION` for the identical
/// `rx_id_proposal` -- rejected by the mock (Fix N-1's own `ERR_NOT_UNIQUE`
/// simulation) -- leaving `comm_started == true` but the service's own
/// bookkeeping stuck at `Requested`, unable to send on or tear down a
/// connection that was, in reality, still genuinely established.
///
/// **Deterministic construction:** both RPCs are issued concurrently
/// (`tokio::join!` over cloned clients, the same technique this file's own
/// `stopcomm_serializes_against_a_racing_repeat_message_start` uses) rather
/// than forced via a mock backdoor -- the server genuinely has the
/// opportunity, though not a forced guarantee, to accept both before the
/// first's own establishment (normally resolving within a single
/// `POLL_INTERVAL_MS` tick) completes. The assertion checks the CORRECTNESS
/// OUTCOME, which must hold regardless of which RPC's own `TxItem::
/// StartComm` the poll task happens to dequeue first: exactly one of the
/// two COPs finishes cleanly (no error event) and the other finishes with
/// `PduErrEvtInitError`; the connection that DID win keeps working
/// afterward (its established TX-ID still frames a `CoptSendrecv`
/// correctly).
///
/// Confirmed to catch the regression by temporarily reverting the
/// `comm_started` recheck this fix adds (restoring the unconditional
/// `Requested`-phase write) and observing this test fail (both COPs
/// finishing with no error event, one of them having silently clobbered the
/// other's established connection), then restoring the fix.
#[tokio::test]
#[serial]
async fn concurrent_startcomm_on_the_same_cll_rejects_the_loser_without_disturbing_the_winner() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let mut client_a = client.clone();
    let mut client_b = client.clone();
    let req = StartComPrimitiveRequest {
        cop_tag: None,
        cll_handle: Some(cll_handle),
        cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
        cop_data: vec![],
        cop_ctrl_data: None,
    };
    let fut_a = client_a.start_com_primitive(req.clone());
    let fut_b = client_b.start_com_primitive(req);
    let (result_a, result_b) = tokio::join!(fut_a, fut_b);

    // Both RPCs must be ACCEPTED synchronously (the race this fix targets
    // happens after acceptance, during dispatch) -- a `FailedPrecondition`
    // rejection here would mean the RPC-level pre-flight check alone closed
    // the race before it could even reach the dispatch-time fix, which is a
    // valid but different (and untested elsewhere) outcome this test is not
    // about; skip the rest if so rather than asserting a specific winner.
    let (Ok(resp_a), Ok(resp_b)) = (result_a, result_b) else {
        drop(events);
        server.shutdown().await;
        return;
    };
    let cop_a = resp_a
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    let cop_b = resp_b
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    let mut finished = std::collections::HashSet::new();
    let mut errored = std::collections::HashSet::new();
    wait_for_event(&mut events, 3000, |item| {
        match &item.data {
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                if let Some(h) = item.cop_handle {
                    finished.insert(h);
                }
            }
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtInitError as i32 =>
            {
                if let Some(h) = item.cop_handle {
                    errored.insert(h);
                }
            }
            _ => {}
        }
        finished.len() == 2
    })
    .await;
    assert_eq!(
        finished,
        std::collections::HashSet::from([cop_a, cop_b]),
        "both concurrent CoptStartcomm COPs must finish (not hang or get cancelled)"
    );
    assert_eq!(
        errored.len(),
        1,
        "exactly one of the two concurrent COPs must be rejected with PduErrEvtInitError -- \
         either both silently succeeded (the pre-fix regression: the loser clobbered the \
         winner's established connection) or neither did (errored={errored:?})"
    );
    let loser = *errored.iter().next().unwrap();
    let winner = if loser == cop_a { cop_b } else { cop_a };
    assert_ne!(loser, winner);

    // The winner's own established connection must still be fully healthy
    // afterward -- unaffected by the loser's rejected duplicate attempt.
    drop(events);
    send_data(&mut client, cll_handle, vec![0xAA, 0xBB, 0xCC], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written,
        vec![0x10, 0x00, 0x03, 0x21, 0xAA, 0xBB, 0xCC],
        "the winning CoptStartcomm's own established TX-ID should still correctly frame a \
         CoptSendrecv payload, unaffected by the loser's rejected duplicate attempt"
    );

    server.shutdown().await;
}

/// Item 2: five sibling CLLs sharing one physical channel (same resource id/
/// pins) each request a connection with a distinct RX-ID -- the first four
/// establish (`PduCopstFinished`, no error event, and COMM_STARTED
/// reachable); the fifth is rejected via the mock's own `CONNECTION_LOST`
/// (reason `0xD8`) indication, surfaced as `PduErrEvtRscLocked` followed by
/// `PduCopstFinished` (ADR-188 Decision item 2 step 5's `0xD8` mapping) --
/// never `PduCopstCancelled`.
#[tokio::test]
#[serial]
async fn connection_rejected_when_all_four_slots_are_full() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let mut cll_handles = Vec::new();
    for (i, rx_id) in [0x0100u32, 0x0101, 0x0102, 0x0103, 0x0104]
        .into_iter()
        .enumerate()
    {
        let cll_handle = create_and_connect_tp20_cll(&mut client, rx_id, i as u64 + 1).await;
        cll_handles.push(cll_handle);
    }
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "all five CLLs should share one physical channel (same resource id/pins)"
    );

    for &cll_handle in &cll_handles[..4] {
        let mut events = client
            .subscribe_event(vci_service_interface::SubscribeEventRequest {
                handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("subscribe_event should succeed")
            .into_inner();
        start_comm(&mut client, cll_handle)
            .await
            .expect("start_com_primitive(CoptStartcomm) should succeed");
        assert!(
            wait_for_event(&mut events, 2000, |item| matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ))
            .await,
            "the first four connection requests should each establish"
        );
    }

    let fifth = cll_handles[4];
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(fifth)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, fifth)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "the fifth CoptStartcomm should finish (not hang or get cancelled)"
    );
    assert!(
        saw_rsc_locked,
        "the fifth connection request should be rejected with PduErrEvtRscLocked (clause 19's \
         0xD8 'temporarily no resources are free')"
    );
    drop(events);

    server.shutdown().await;
}

/// Item 3: the five minted `PARAM_TP20_*` ComParams round-trip through
/// `SetComParam`/`GetComParam` before `CoptStartcomm` ever runs.
#[tokio::test]
#[serial]
async fn five_comparams_round_trip_via_set_and_get() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_tp20_cll(&mut client, TP2_0_RESOURCE_ID, TP2_0_PINS, 1).await;

    let cases = [
        (CP_TP20_CHANNEL_SETUP_CAN_ID, 0x0000_0700u32),
        (CP_TP20_DESTINATION_ADDRESS, 0x10),
        (CP_TP20_TX_ID_PROPOSAL, 0x0300),
        (CP_TP20_RX_ID_PROPOSAL, 0x0321),
        (CP_TP20_APPLICATION_TYPE, 1),
    ];
    for &(com_param_id, value) in &cases {
        set_com_param_unum32(&mut client, cll_handle, com_param_id, value).await;
    }
    for &(com_param_id, value) in &cases {
        assert_eq!(
            get_com_param_unum32(&mut client, cll_handle, com_param_id).await,
            value,
            "ComParam {com_param_id:#06x} should round-trip"
        );
    }

    server.shutdown().await;
}

/// Item 4a (clause 19.2.2's closed pin set): the one documented pair (6/14)
/// is accepted -- already exercised by every successful connect test above
/// (`create_and_connect_tp20_cll` always uses `TP2_0_PINS`); this test only
/// adds the explicit positive assertion on the packed `CONFIG_J1962_PINS`
/// value.
#[tokio::test]
#[serial]
async fn closed_pin_pair_accepts_pins_6_and_14() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_060E,
        "pins 6/14 should pack to 0x0000_060E (primary 6, secondary 14/0x0E)"
    );

    server.shutdown().await;
}

/// Item 4b: any pin pair other than 6/14 is rejected -- clause 19.2.2
/// documents exactly one pair, unlike FT-CAN's own two documented pairs.
#[tokio::test]
#[serial]
async fn closed_pin_pair_rejects_any_other_pin_pair() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    // Pin validation for a resource-id route resolves at
    // `CreateComLogicalLink` time (`names::resolve_pin_selection`), not
    // `ConnectComLogicalLink` -- unlike the raw-protocol-id direct-
    // construction route other tests in this file exercise.
    let err = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(TP2_0_RESOURCE_ID, &[(1, "HI"), (9, "LOW")]),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err("creating with pins 1/9 (not the one documented pair 6/14) should fail");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// Item 5 (ADR-188's own stated test-coverage goal): two CLLs share one
/// physical channel (same resource id/pins) and each independently
/// establishes and tears down its own connection, distinguished only by
/// `CP_TP20RxIdProposal`.
#[tokio::test]
#[serial]
async fn two_clls_share_one_physical_channel_and_independently_connect_and_teardown() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let cll_b = create_and_connect_tp20_cll(&mut client, 0x0322, 2).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and CLL B should share one physical channel (same resource id/pins)"
    );

    for &cll_handle in &[cll_a, cll_b] {
        let mut events = client
            .subscribe_event(vci_service_interface::SubscribeEventRequest {
                handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("subscribe_event should succeed")
            .into_inner();
        start_comm(&mut client, cll_handle)
            .await
            .expect("start_com_primitive(CoptStartcomm) should succeed");
        assert!(
            wait_for_event(&mut events, 2000, |item| matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ))
            .await,
            "each CLL's own CoptStartcomm should independently finish"
        );
    }

    // Disconnecting A must not disturb B: the physical channel survives
    // (ref_count > 0), and B remains healthy.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect(
            "disconnecting CLL A while CLL B still shares the physical channel should not error \
             or hang",
        );
    assert_eq!(
        server.backdoor.disconnect_count(),
        0,
        "the physical channel must survive CLL A's disconnect while CLL B still holds it"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_b, CP_TP20_RX_ID_PROPOSAL).await,
        0x0322,
        "CLL B should remain healthy (still able to GetComParam) after CLL A's disconnect"
    );

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect("disconnecting the last CLL on this channel should succeed");
    assert_eq!(server.backdoor.disconnect_count(), 1);

    server.shutdown().await;
}

/// Item 6 (the ADR-184 routing shape ADR-188 itself names): an inbound frame
/// addressed to CLL A's own established RX-ID is delivered only to A, never
/// to sibling CLL B sharing the same physical channel -- and the leading
/// 4-byte RX-ID prefix is split off into `extra_info.header_bytes`, never
/// leaked into `data_bytes` (mirroring `header_footer_len`'s own CAN-arm
/// shape).
#[tokio::test]
#[serial]
async fn rx_frame_routes_only_to_the_matching_established_connection() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let cll_b = create_and_connect_tp20_cll(&mut client, 0x0322, 2).await;

    for &cll_handle in &[cll_a, cll_b] {
        let mut events = client
            .subscribe_event(vci_service_interface::SubscribeEventRequest {
                handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("subscribe_event should succeed")
            .into_inner();
        start_comm(&mut client, cll_handle)
            .await
            .expect("start_com_primitive(CoptStartcomm) should succeed");
        assert!(
            wait_for_event(&mut events, 2000, |item| matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ))
            .await,
            "each CLL's own CoptStartcomm should independently finish"
        );
    }

    arm_receive_only_monitor(&mut client, cll_a).await;
    arm_receive_only_monitor(&mut client, cll_b).await;

    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let frame = can_frame(0x0321, &payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::PROTOCOL_TP2_0_PS);

    let result = wait_for_result_data(&mut client, cll_a).await;
    assert_result_data(&result, &[0x00, 0x00, 0x03, 0x21], &[], &payload);

    assert_no_result_data(
        &mut client,
        cll_b,
        "a frame addressed to CLL A's own RX-ID must never route to sibling CLL B",
    )
    .await;

    server.shutdown().await;
}

/// Item 6b (Codex review fix, PR #97, round 9, Fix M): a `CP_Loopback`-
/// enabled write's own device-generated echo -- addressed by CLL A's own
/// established TX-ID, not its RX-ID -- routes back to CLL A itself, not
/// dropped and not misdelivered to sibling CLL B. Synthesized directly via
/// `inject_rx_with_status` with the `TX_MSG_TYPE` bit set (mirrors the
/// mock's own `PassThruWriteMsgs` loopback-echo shape: the echo carries the
/// written frame's own 4-byte TX-ID prefix unchanged) rather than driving a
/// real `CP_Loopback`-enabled `CoptSendrecv`, the same "synthesize the
/// specific RxStatus shape directly" convention `rx_header_split.rs`'s own
/// `iso15765_loopback_only_reports_rx_flag_0x01` uses.
#[tokio::test]
#[serial]
async fn loopback_echo_of_own_write_routes_to_the_originating_connection() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let cll_b = create_and_connect_tp20_cll(&mut client, 0x0322, 2).await;

    for &cll_handle in &[cll_a, cll_b] {
        let mut events = client
            .subscribe_event(vci_service_interface::SubscribeEventRequest {
                handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("subscribe_event should succeed")
            .into_inner();
        start_comm(&mut client, cll_handle)
            .await
            .expect("start_com_primitive(CoptStartcomm) should succeed");
        assert!(
            wait_for_event(&mut events, 2000, |item| matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ))
            .await,
            "each CLL's own CoptStartcomm should independently finish"
        );
    }

    arm_receive_only_monitor(&mut client, cll_a).await;
    arm_receive_only_monitor(&mut client, cll_b).await;

    // CLL A's own established TX-ID (the mock's `rx_id | 0x1000_0000`
    // assignment for rx_id 0x0321 -- the same value `connection_establishes_
    // and_tears_down_cleanly` observes on the wire for a real send).
    let payload = vec![0xAA, 0xBB, 0xCC];
    let echo = can_frame(0x1000_0321, &payload);
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &echo,
        j2534_0404::PROTOCOL_TP2_0_PS,
        0x0000_0001, // TX_MSG_TYPE
    );

    let result = wait_for_result_data(&mut client, cll_a).await;
    // ADR-098: `TX_MSG_TYPE` (RxStatus bit 0) is one of the 5 low bits
    // `rx_flag` carries -- this frame is not a Normal Message, so
    // `assert_result_data_with_rx_flag` (not `assert_result_data`, which
    // asserts `rx_flag` is EMPTY) is the correct helper here.
    assert_result_data_with_rx_flag(
        &result,
        &[0x10, 0x00, 0x03, 0x21],
        &[],
        &payload,
        &[0x00, 0x00, 0x00, 0x01],
    );

    assert_no_result_data(
        &mut client,
        cll_b,
        "CLL A's own loopback echo must never route to sibling CLL B",
    )
    .await;

    server.shutdown().await;
}

/// Item 7 (regression pin + documented residual for the concurrency-bug
/// fix to `run_tp20_connection_request`'s pre-insert `owned_by_a_live_
/// sibling` ownership check, `events_tp20_connection.rs` -- the
/// `events_j1939_claim.rs` `owned_by_a_live_sibling`/cleanup-gate precedent
/// it mirrors).
///
/// **What this test can and cannot prove, and why:** the fix's own
/// ownership check can only ever observe a live sibling CLL's own
/// still-PENDING `IOCTL_REQUEST_CONNECTION` -- `SharedChannel::
/// tp20_connections` removes an attempt's entry as soon as it resolves
/// either way (unlike J1939's `j1939_claims`, which retains a successful
/// claim for the CLL's whole session), so a collision against an
/// already-ESTABLISHED sibling (what this test constructs, below) is NOT
/// something the fix rejects -- and, separately, this codebase gives each
/// physical channel exactly one poll task
/// (`events::spawn_channel_poll_task`) that processes a queued
/// `TxItem::StartComm` -- including the ENTIRE bounded
/// `run_tp20_connection_request` wait -- to completion before dequeuing the
/// next one, so two sibling CLLs sharing one physical channel can never
/// have overlapping "both still pending" windows via the ordinary
/// client-driven RPC path this `grpc_mock` harness drives either. Both
/// escape hatches this fix's own brief allowed for ("attempt the
/// genuinely-pending-window case first; if infeasible, fall back to the
/// simpler already-established-sibling case") turn out to be unreachable
/// via THIS harness for the same underlying reason -- see this module's
/// own doc comment and the Prioritized
/// Backlog for the full analysis. The fix's own decision logic
/// (`tp20_rx_id_unavailable_for`, renamed from `tp20_rx_id_owned_by_a_live_
/// sibling` when a 7th review round extended it to also cover an
/// `abandoned`-quarantine rejection reason -- see this module's own doc
/// comment and `docs/implementation-notes.md`'s TP2.0 narrative section) IS
/// unit-tested directly in `events_tp20_connection.rs`'s own `#[cfg(test)]
/// mod tests` -- that is
/// the actual proof of Fix 1's correctness; this test instead pins the
/// established-sibling collision case's own end-to-end behavior, which a
/// LATER fix (round 11, below) made correct by simulating the device's own
/// native rejection at the mock level -- Fix 1 itself never rejects this
/// case (see above), so this test still isn't proof of Fix 1's own
/// ownership-check logic; it just keeps pinning whatever THIS case's real
/// behavior is so a future regression there is still caught.
///
/// CLL B proposing the SAME `CP_TP20RxIdProposal` as CLL A's own
/// already-ESTABLISHED connection is now correctly REJECTED (Codex review
/// fix, PR #97, round 11): the mock's `IOCTL_REQUEST_CONNECTION` handler
/// now simulates clause 19.3.3.2's native `ERR_NOT_UNIQUE` rejection for
/// this device-level collision instead of silently overwriting the
/// existing slot -- closing a previously-documented mock-fidelity gap
/// (the Prioritized Backlog). This test pins
/// both that CLL B's colliding `CoptStartcomm` finishes with
/// `PduErrEvtInitError` (never hangs or gets cancelled), and that CLL A's
/// own established TX-ID framing keeps working correctly afterward,
/// unaffected by CLL B's rejected attempt.
#[tokio::test]
#[serial]
async fn second_cll_reusing_an_established_siblings_rx_id_is_rejected_and_does_not_disturb_it() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's own connection should establish first"
    );
    drop(events_a);

    // CLL B shares the same physical channel (same resource id/pins) and
    // proposes the SAME rx_id CLL A already established under.
    let cll_b = create_and_connect_tp20_cll(&mut client, 0x0321, 2).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and CLL B should share one physical channel"
    );

    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_init_error = false;
    wait_for_event(&mut events_b, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtInitError as i32 =>
            {
                saw_init_error = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "CLL B's colliding CoptStartcomm should finish (not hang or get cancelled)"
    );
    assert!(
        saw_init_error,
        "CLL B's colliding CoptStartcomm should be rejected with PduErrEvtInitError -- the \
         client-visible mapping of the mock's now-simulated clause 19.3.3.2 ERR_NOT_UNIQUE"
    );
    drop(events_b);

    // CLL A's own connection must remain fully healthy: still routed under
    // its own established TX-ID, unaffected by CLL B's rejected collision
    // attempt -- pins that Fix 1's own cleanup-block changes (the
    // ownership-gated `tp20_connections.remove`, the
    // `tp20_connection_results.remove` drain) did not disturb this.
    send_data(&mut client, cll_a, vec![0xAA, 0xBB, 0xCC], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written,
        vec![0x10, 0x00, 0x03, 0x21, 0xAA, 0xBB, 0xCC],
        "CLL A's own established TX-ID framing should be unaffected by CLL B's later registration"
    );

    server.shutdown().await;
}

/// Item 8 (Codex review regression, PR #97, ADR-188 Fix B): a TP2.0 CLL
/// whose own connection is NOT `Established` must never wildcard-receive
/// traffic addressed to a sibling CLL's own established connection on the
/// same shared physical channel.
///
/// `events_rx_routing.rs::build_cll_rx_entries` used to only push a
/// `unique_resp_ids` routing entry for a TP2.0 CLL once its connection
/// reached `Tp20ConnectionPhase::Established` -- a CLL that is merely
/// connected-but-`Requested`, `Lost`, or has no `tp20_connection` at all
/// contributed nothing, leaving `unique_resp_ids` empty, which
/// `route_frame`'s own "empty table -> no table configured -> deliver every
/// frame unconditionally" fallback (the correct behavior for KWP/J1850,
/// which have no per-CLL routing table concept) then wildcard-delivered to.
///
/// **What this test can and cannot prove, and why (mirrors this file's own
/// Item 7 precedent above for the identical class of harness limit):** the
/// actual routing-construction fix
/// (`events_rx_routing::build_cll_rx_entries`'s per-TP2.0-CLL sentinel
/// entry) is unit-tested directly, against a hand-built non-`Established`
/// `LogicalLinkState`, in
/// `events_build_cll_rx_entries_tests.rs::tp20_non_established_cll_gets_an_unmatchable_sentinel_entry_not_an_empty_table`
/// -- that is the actual proof of Fix B's own correctness. An end-to-end
/// proof through this `grpc_mock` harness (arm a real receive-only monitor
/// on a non-`Established` CLL B, so a wrongly-delivered frame would surface
/// as `ResultData`, then check B never gets it) turns out to be
/// unconstructible here: Fix A's own correctness requirement (`CoptSendrecv`
/// -- including a receive-only monitor's own required TX-header resolution,
/// `resolve_send_recv_tx`/`build_tx_message` -- rejects outright on ANY
/// non-`Established` TP2.0 CLL) means a monitor can only ever be armed WHILE
/// B is still `Established`; and `CoptStopcomm` (`rpc_primitive.rs`'s own
/// `cops_to_cancel` sweep, the "cancel all queued primitives for this link"
/// block) unconditionally cancels every other COP on the CLL, including an
/// already-detached tier-2 (`RegistrantTier::ReceiveOnly`) receive-only
/// registrant -- unlike `PDU_IOCTL_CLEAR_TX_QUEUE`, which explicitly
/// excludes a live tier-2 registrant from its own equivalent sweep
/// (`rpc_misc.rs::ioctl_clear_tx_queue`'s own `detached_tier2_cops`
/// exclusion). So the one RPC that can move a TP2.0 CLL out of
/// `Established` without fully disconnecting it (`CoptStopcomm`) always
/// takes any registrant armed on it down too, leaving no way to arm a real
/// listener on a non-`Established` TP2.0 CLL via this crate's RPC surface
/// today. This test instead pins the currently-correct, unaffected-by-this-
/// fix end-to-end behavior around that same `CoptStopcomm` transition (A's
/// own delivery keeps working; B's own health/connectivity survives) so a
/// future regression THERE is still caught, and documents the fix's real
/// proof location above rather than asserting something this harness cannot
/// actually distinguish from a false negative (an unbound content frame is
/// discarded outright regardless of routing when a CLL has no active
/// registrant at all, ADR-100 Decision §5 S8 -- `assert_no_result_data` on a
/// registrant-less B would pass whether Fix B is present or not).
#[tokio::test]
#[serial]
async fn non_established_sibling_does_not_disrupt_an_established_peers_delivery() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let cll_b = create_and_connect_tp20_cll(&mut client, 0x0322, 2).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and CLL B should share one physical channel (same resource id/pins)"
    );

    for &cll_handle in &[cll_a, cll_b] {
        let mut events = client
            .subscribe_event(vci_service_interface::SubscribeEventRequest {
                handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("subscribe_event should succeed")
            .into_inner();
        start_comm(&mut client, cll_handle)
            .await
            .expect("start_com_primitive(CoptStartcomm) should succeed");
        assert!(
            wait_for_event(&mut events, 2000, |item| matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ))
            .await,
            "each CLL's own CoptStartcomm should independently finish"
        );
    }

    // Move CLL B out of `Established` (`tp20_connection` reverts to `None`,
    // `events.rs`'s CoptStopcomm teardown block) while it stays connected to
    // the shared physical channel -- CLL A remains fully established and
    // unaffected.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_b),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) on CLL B should succeed");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    arm_receive_only_monitor(&mut client, cll_a).await;

    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let frame = can_frame(0x0321, &payload);
    server
        .backdoor
        .inject_rx(MOCK_CHANNEL_ID, &frame, j2534_0404::PROTOCOL_TP2_0_PS);

    let result = wait_for_result_data(&mut client, cll_a).await;
    assert_result_data(&result, &[0x00, 0x00, 0x03, 0x21], &[], &payload);

    // CLL B stays healthy (still able to GetComParam) after its own
    // CoptStopcomm, unaffected by the routing-construction fix.
    assert_eq!(
        get_com_param_unum32(&mut client, cll_b, CP_TP20_RX_ID_PROPOSAL).await,
        0x0322,
        "CLL B should remain healthy (still able to GetComParam) after its own CoptStopcomm"
    );

    server.shutdown().await;
}

/// ADR-188 fix (edge-case-hunter, PR #97 round following the initial TX-ID
/// fix): `CoptSendrecv` has no `comm_started` precondition gate the way
/// `CoptStartcomm`/`CoptStopcomm` do, so a `CoptSendrecv` can bind (capturing
/// this CLL's currently-`Established` TX-ID) and be enqueued strictly AFTER
/// a `CoptStopcomm`'s own `StartComPrimitive` call has already returned --
/// escaping that `CoptStopcomm`'s own `cops_to_cancel` sweep entirely (that
/// sweep only cancels COPs already present in `primitives` at the moment
/// `CoptStopcomm` ITSELF was called). Without the fix, this `CoptSendrecv`
/// would still transmit later, using the now-stale, already-torn-down
/// TX-ID -- because `tp20_established_tx_id` was resolved exactly once, at
/// `StartComPrimitive` bind time, and never re-verified at actual dispatch
/// time.
///
/// Deterministic construction (`PDU_IOCTL_SUSPEND_TX_QUEUE`/
/// `RESUME_TX_QUEUE`, the same technique `j1939.rs`'s own
/// `coptupdateparam_execution_time_check_catches_a_claim_that_lands_after_
/// enqueue` uses for the structurally identical J1939 hazard): suspend this
/// CLL's own TX queue once the connection is `Established`, so both a
/// `CoptStopcomm` (cop_data empty -- no final message, so its dispatch goes
/// straight to the unconditional `tp20_connection.take()` teardown) and a
/// following `CoptSendrecv` are accepted synchronously and diverted into
/// `tx_held` (FIFO) without executing. `CoptSendrecv`'s own bind-time
/// snapshot resolves against the CLL's `tp20_connection` -- still
/// `Established` at that moment, since `CoptStopcomm`'s queued teardown has
/// not run yet. Resuming drains `tx_held` strictly FIFO: `CoptStopcomm`
/// dispatches first (tearing the connection down), then `CoptSendrecv`
/// dispatches against an already-torn-down connection.
///
/// With the fix, `handle_send_recv`'s transmit-time re-check re-reads the
/// CLL's CURRENT `tp20_connection` and finds it gone, so the queued
/// `CoptSendrecv` is cancelled (`PduCopstCancelled`) rather than
/// transmitted -- proven both by the event and by the wire: no message ever
/// reaches the mock (`written_count` stays at the pre-suspend baseline).
#[tokio::test]
#[serial]
async fn coptsendrecv_queued_behind_a_racing_stopcomm_teardown_is_cancelled_not_sent_stale() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish (PduCopstFinished) once the connection establishes"
    );

    let baseline_written = server.backdoor.written_count(MOCK_CHANNEL_ID);

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    let resume_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_RESUME_TX_QUEUE").await;

    io_ctl_cll(&mut client, cll_handle, suspend_id, None, false)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    // Accepted synchronously (siphoned into tx_held while suspended); no
    // final message (cop_data empty), so its own eventual dispatch goes
    // straight to the unconditional `tp20_connection.take()` teardown with
    // no P3-gap wait first.
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect(
            "start_com_primitive(CoptStopcomm) should be accepted synchronously (siphoned into \
             tx_held while suspended)",
        );

    // Bound HERE, while the CLL's `tp20_connection` is still genuinely
    // `Established` (the queued CoptStopcomm above has not dispatched its
    // teardown yet) -- this is exactly the enqueue-time snapshot the bug
    // captures and never re-verifies.
    let send_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0xAA, 0xBB, 0xCC],
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
            "start_com_primitive(CoptSendrecv) should be accepted synchronously (siphoned into \
             tx_held behind the queued CoptStopcomm while suspended)",
        )
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    let _ = send_cop_handle;

    io_ctl_cll(&mut client, cll_handle, resume_id, None, false)
        .await
        .expect("PDU_IOCTL_RESUME_TX_QUEUE should succeed");

    // CoptStopcomm dispatches first (FIFO) and finishes, tearing down
    // `tp20_connection` before CoptSendrecv is ever popped from `tx_held`.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStopcomm should finish and tear down the TP2.0 connection before CoptSendrecv \
         dispatches"
    );

    // CoptSendrecv dispatches next, against the now-torn-down connection --
    // the fix under test must cancel it rather than transmit with the
    // stale TX-ID.
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "the queued CoptSendrecv must be cancelled once its CLL's TP2.0 connection is torn down \
         by the racing CoptStopcomm's own queued teardown -- otherwise it transmits with a \
         stale, already-torn-down TX-ID"
    );

    // Wire-output proof: no message ever reached the mock for the cancelled
    // CoptSendrecv (CoptStopcomm itself sent no final message -- cop_data was
    // empty).
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        baseline_written,
        "the cancelled CoptSendrecv must never reach the wire with a stale TP2.0 TX-ID"
    );

    drop(events);
    server.shutdown().await;
}

/// Item 9 (Fix D, Codex review, PR #97, P1): `CoptStopcomm` on a TP2.0 CLL
/// must stop this CLL's own live repeat message slots BEFORE tearing down
/// the native connection -- `tx_header::build_tx_message`'s TP2.0 arm bakes
/// the connection's established TX-ID into a repeat slot's payload once, at
/// `PDU_IOCTL_START_REPEAT_MESSAGE` time, and never re-resolves it per
/// retransmission cycle, so a still-live slot left running past teardown
/// would have the adapter keep autonomously retransmitting that now-stale
/// TX-ID as raw, non-connection traffic. `handle_stop_comm`
/// (`events.rs`) now calls the same `stop_repeat_slots_for_cll`/
/// `record_leaked_repeat_slots` best-effort pair the SAE J1939 "relinquished
/// address" teardown path already uses (ADR-180 Decision 10), immediately
/// before the native `IOCTL_TEARDOWN_CONNECTION` call.
#[tokio::test]
#[serial]
async fn stopcomm_stops_this_clls_own_repeat_slots_before_native_teardown() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish (PduCopstFinished) once the connection establishes"
    );

    // Start a repeat message slot while the connection is Established --
    // `rpc_misc.rs::ioctl_start_repeat_message` requires this (composes the
    // established TX-ID via `tx_header::build_tx_message`'s TP2.0 arm, same
    // as an ordinary `CoptSendrecv`). `mask_data`/`pattern_data` are each
    // prefixed with `tx_header::response_header_bytes`'s own TP2.0 arm (Fix
    // H, ADR-188): a 4-byte big-endian established-RX-ID header with an
    // all-`0xFF` mask, so the resulting PassThruMessage is 8 bytes each
    // (4-byte header + these 4 client bytes) -- comfortably within the
    // protocol's `4..=4096`-byte TX message size range regardless.
    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let setup = DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup(
                5000,
                0,
                &[0x01, 0x02],
                &[0x00, 0x00, 0x00, 0x00],
                &[0x00, 0x00, 0x00, 0x00],
                &[],
            ),
        })),
    };
    let output = io_ctl_cll(&mut client, cll_handle, start_id, Some(setup), true)
        .await
        .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed on an Established TP2.0 CLL");
    let msg_id = match output.and_then(|d| d.data) {
        Some(data_item::Data::Unum32Value(msg_id)) => msg_id,
        other => panic!(
            "PDU_IOCTL_START_REPEAT_MESSAGE should return a Unum32Value MsgId, got {other:?}"
        ),
    };
    assert!(
        server
            .backdoor
            .repeat_message_exists(MOCK_CHANNEL_ID, msg_id),
        "the repeat slot should be live immediately after START"
    );

    // CoptStopcomm: cop_data empty, so no final message -- dispatch goes
    // straight to the repeat-slot cleanup and connection teardown block.
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
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStopcomm should finish"
    );

    assert!(
        !server
            .backdoor
            .repeat_message_exists(MOCK_CHANNEL_ID, msg_id),
        "CoptStopcomm must stop this CLL's own repeat message slot before/alongside tearing \
         down the native TP2.0 connection -- otherwise the adapter keeps autonomously \
         retransmitting the now-stale established TX-ID as raw traffic after teardown"
    );

    drop(events);
    server.shutdown().await;
}

/// Fix I (Codex review, PR #97, ADR-188): `handle_stop_comm`'s TP2.0 arm now
/// holds `shared_channels` outermost across its entire clear-connection +
/// sweep-repeat-slots + native-teardown span, serializing against
/// `ioctl_start_repeat_message`'s (`rpc_misc.rs`) own identical
/// `shared_channels` hold -- closing a race where a `PDU_IOCTL_START_
/// REPEAT_MESSAGE` could land its own registration into `repeat_message_ids`
/// strictly AFTER `CoptStopcomm`'s repeat-slot sweep already ran (finding
/// nothing, since the slot did not exist yet), leaving a freshly-created
/// slot autonomously retransmitting the connection's now-stale established
/// TX-ID after native teardown.
///
/// **Deterministic construction:** unlike this file's other racing
/// regressions (`coptsendrecv_queued_behind_a_racing_stopcomm_teardown_is_
/// cancelled_not_sent_stale`, Item 10 above), there is no `PDU_IOCTL_
/// SUSPEND_TX_QUEUE`-siphon or `__mock_set_tp20_no_indication`-style hook
/// that can force `PDU_IOCTL_START_REPEAT_MESSAGE` (a synchronous IoCtl
/// RPC handled directly, never queued through `tx_held`) to pause
/// mid-flight inside the exact window between `CoptStopcomm`'s
/// `tp20_connection.take()` and its own repeat-slot sweep -- the same "no
/// natural preemption point in this harness" residual Item 10's own doc
/// comment records for the structurally analogous `self_still_live`
/// recheck (tracked as a P3 backlog entry, mirroring that residual).
/// Instead, both RPCs are issued concurrently (`tokio::join!` over cloned
/// client handles, the same "land only microseconds apart" technique
/// `tester_present_send_type.rs`'s own back-to-back `StartComPrimitive`
/// race uses) so the server genuinely has an opportunity -- though not a
/// forced guarantee -- to interleave `handle_stop_comm`'s poll-task
/// dispatch against this RPC handler's own execution. The assertion below
/// checks the CORRECTNESS OUTCOME, which must hold regardless of which side
/// actually wins whatever interleaving occurs on a given run: either the
/// START fails (the connection tore down before it could resolve an
/// established TX-ID), or it succeeds and registers a slot that StopComm's
/// sweep -- now guaranteed to run after any such registration completes,
/// under the shared lock -- also stops. No repeat slot may survive this
/// exchange still carrying the stale TX-ID.
///
/// **Confirmed limitation (implementer verification, not just a theoretical
/// caveat):** run 40 times in a row against a deliberately reverted,
/// pre-Fix-I `handle_stop_comm` (the un-widened `shared_channels` span),
/// this test passed on every run -- the `tokio::join!` concurrent-issuance
/// technique alone never actually landed the specific interleaving Fix I
/// closes, so this test does NOT fail-then-pass the way the fixes above do.
/// It is kept anyway as a standing correctness-outcome sanity check (it
/// would still catch a DIFFERENT regression that broke the outcome
/// invariant itself), but it is not proof the race window is closed --
/// see the Prioritized Backlog entry this
/// doc comment cites for the tracked residual.
#[tokio::test]
#[serial]
async fn stopcomm_serializes_against_a_racing_repeat_message_start() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish (PduCopstFinished) once the connection establishes"
    );

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let setup = DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup(
                5000,
                0,
                &[0x01, 0x02],
                &[0x00, 0x00, 0x00, 0x00],
                &[0x00, 0x00, 0x00, 0x00],
                &[],
            ),
        })),
    };

    let mut client_start = client.clone();
    let mut client_stop = client.clone();
    let start_fut = io_ctl_cll(&mut client_start, cll_handle, start_id, Some(setup), true);
    let stop_fut = client_stop.start_com_primitive(StartComPrimitiveRequest {
        cop_tag: None,
        cll_handle: Some(cll_handle),
        cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
        cop_data: vec![],
        cop_ctrl_data: None,
    });
    let (start_result, stop_result) = tokio::join!(start_fut, stop_fut);
    stop_result.expect("start_com_primitive(CoptStopcomm) should be accepted");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStopcomm should finish"
    );

    if let Ok(output) = start_result {
        let msg_id = match output.and_then(|d| d.data) {
            Some(data_item::Data::Unum32Value(msg_id)) => msg_id,
            other => panic!(
                "PDU_IOCTL_START_REPEAT_MESSAGE should return a Unum32Value MsgId, got {other:?}"
            ),
        };
        assert!(
            !server
                .backdoor
                .repeat_message_exists(MOCK_CHANNEL_ID, msg_id),
            "a repeat slot that won its race against a racing CoptStopcomm's connection \
             teardown must still be stopped by StopComm's own repeat-slot sweep -- never left \
             autonomously retransmitting the connection's now-stale established TX-ID after \
             native teardown"
        );
    }

    drop(events);
    server.shutdown().await;
}

/// Item 10 (Codex review, PR #97, 4th round): the mid-wait staleness check
/// inside `run_tp20_connection_request`'s bounded wait loop
/// (`events_tp20_connection.rs`) must best-effort `IOCTL_TEARDOWN_CONNECTION`
/// a request whose native `IOCTL_REQUEST_CONNECTION` call already succeeded
/// before this CLL went stale (disconnected) while still waiting for the
/// resulting `CONNECTION_ESTABLISHED`/`_LOST` indication -- otherwise the
/// native slot leaks until the device's own maintenance timeout even though
/// the service already knows this CLL is gone.
///
/// **Deterministic construction:** a bare back-to-back
/// `StartComPrimitive`-then-`DisconnectComLogicalLink` RPC race cannot land
/// reliably here -- the mock's `IOCTL_REQUEST_CONNECTION` handler queues its
/// `CONNECTION_ESTABLISHED` indication synchronously, so the wait loop's own
/// bounded wait normally resolves within a single `POLL_INTERVAL_MS` tick
/// (~10ms), well under this harness's own measured gRPC round-trip overhead
/// ceiling (`harness.rs`'s `GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS`, ADR-149) --
/// the same "a bare back-to-back RPC race can never land reliably" problem
/// `j1939.rs`'s own `spontaneous_reclaim_aborts_on_a_pending_stopcomm_
/// without_issuing_a_claim` documents for the structurally identical J1939
/// claim-loop case. The fix is `__mock_set_tp20_no_indication` (the TP2.0
/// analogue of `j1939.rs`'s own `__mock_set_j1939_claim_no_indication` /
/// `cancel_com_primitive_during_the_claim_wait_ends_it_promptly_as_
/// cancelled` technique): armed, `IOCTL_REQUEST_CONNECTION` still genuinely
/// allocates a `tp20_connections` slot but never reports
/// `CONNECTION_ESTABLISHED`, so the wait loop genuinely blocks for the rest
/// of its ~2s bounded wait, giving a real, wide, deterministic window.
///
/// **Construction:** three sibling CLLs (S1-S3) establish normally first,
/// occupying three of the mock's four `TP20_MAX_CONNECTIONS_PER_CHANNEL`
/// slots (the same limit `connection_rejected_when_all_four_slots_are_full`,
/// Item 2 above, exercises). CLL A's own request (armed) allocates the
/// fourth. Disconnecting A once its `CoptStartcomm` is confirmed
/// `PduCopstExecuting` (a structural precondition, not a raw sleep) leaves
/// `A`'s own `LogicalLinkState::tp20_connection` at `Requested`, never
/// `Established` -- so `DisconnectComLogicalLink`'s OWN pre-existing
/// best-effort teardown (`rpc_link.rs`, gated on `phase == Established`)
/// does NOT run for it, isolating this test to the NEW mid-loop fix under
/// test alone. This harness has no direct `IOCTL_TEARDOWN_CONNECTION`
/// call-count backdoor (the same limit `tp20_no_indication`'s own field doc
/// documents, mirroring `j1939.rs`'s identical reasoning for
/// `IOCTL_PROTECT_J1939_ADDR`), so the freed slot is proven the same
/// indirect way Item 2 above already does: a fourth sibling CLL S4 then
/// requests its own connection on the same physical channel -- with the
/// fix, CLL A's own slot was freed by the mid-loop teardown, so S4 becomes
/// the (now free) fourth occupant and establishes normally; without the
/// fix, CLL A's slot never freed, so S4's request is the FIFTH and is
/// rejected the identical way Item 2's own fifth CLL is (`PduErrEvtRscLocked`,
/// clause 19's `0xD8`).
///
/// Confirmed to catch the regression by temporarily commenting out just the
/// mid-loop `best_effort_teardown_on_abandon` call in the `!still_on_this_
/// channel` branch (leaving the `break ... Stale` itself intact) and
/// observing this test fail exactly as expected (S4 rejected with
/// `PduErrEvtRscLocked`, proving CLL A's slot was never freed), then
/// restoring it.
///
/// **What this test does NOT cover, and why (see this file's own module doc
/// and the Prioritized Backlog for the residual
/// this leaves):** the SAME round's OTHER fix -- the pre-issue `self_still_
/// live` recheck inside the critical section BEFORE the native
/// `IOCTL_REQUEST_CONNECTION` call is ever issued -- has no analogous
/// deterministic construction available. Unlike this test's own mid-loop
/// case, there is no natural preemption point between `handle_start_comm`'s
/// "Fresh attempt" `Requested`-state write and `run_tp20_connection_
/// request`'s own first `logical_links` lock acquisition a few lines later:
/// both are synchronous, uncontended lock acquisitions with no intervening
/// real `.await` yield, and every `#[tokio::test]` in this suite runs on
/// tokio's default single-threaded (current-thread) runtime, so no other
/// task -- including a concurrent `DisconnectComLogicalLink` RPC handler --
/// can ever be scheduled to run inside that gap. This is the same "no
/// natural preemption point in this harness" residual `j1939.rs`'s own
/// ADR-180 Decision 21 text accepts for the identical reason
/// (`cop_ctrl_cycles.rs`'s `delay_dispatched_item_goes_stale_after_
/// reconnect_on_same_channel` doc comment makes the same point from the
/// other direction: it is constructible there specifically BECAUSE
/// `handle_delay`'s own per-tick `tokio::time::sleep` gives it one). Nor can
/// the SUSPEND_TX_QUEUE-then-resume/disconnect technique substitute: any
/// item sitting in CLL A's own `tx_held` (or still merely enqueued, never
/// dispatched) is unconditionally drained and cancelled by `cancel_link_
/// cops` the moment CLL A disconnects (`should_skip_cancelled_item`'s own
/// implicit-cancel branch then skips it on the next dequeue, checking `l.
/// connected` before `handle_start_comm` -- let alone `run_tp20_connection_
/// request` -- ever runs), so a disconnect landing before dispatch begins
/// can never reach the `self_still_live` check at all; proving "the native
/// call was never issued" that way would pass whether the fix is present or
/// not, not actually exercising it. The pre-issue check's own correctness
/// is reviewed by code inspection instead (see `events_tp20_connection.rs`'s
/// own doc comment on the check itself).
#[tokio::test]
#[serial]
async fn disconnect_mid_wait_after_native_issue_tears_down_the_leaked_slot() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    // S1-S3: three siblings establish normally, occupying three of the
    // mock's four TP2.0 connection slots.
    for (i, rx_id) in [0x0100u32, 0x0101, 0x0102].into_iter().enumerate() {
        let cll_handle = create_and_connect_tp20_cll(&mut client, rx_id, i as u64 + 1).await;
        let mut events = client
            .subscribe_event(vci_service_interface::SubscribeEventRequest {
                handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("subscribe_event should succeed")
            .into_inner();
        start_comm(&mut client, cll_handle)
            .await
            .expect("start_com_primitive(CoptStartcomm) should succeed");
        assert!(
            wait_for_event(&mut events, 2000, |item| matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ))
            .await,
            "sibling {i} should establish normally"
        );
    }
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "all sibling CLLs should share one physical channel (same resource id/pins)"
    );

    // CLL A: the victim. Its own request (armed below) allocates the
    // fourth (last) slot, but the resulting indication is withheld.
    let cll_a = create_and_connect_tp20_cll(&mut client, 0x0103, 4).await;

    server.backdoor.set_tp20_no_indication(true);

    let cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");

    wait_for_cop_executing(&mut client, cop_handle).await;

    // Disconnect CLL A while its own CoptStartcomm is confirmed still
    // actively waiting inside run_tp20_connection_request's bounded wait
    // loop -- the native IOCTL_REQUEST_CONNECTION call already succeeded
    // above, allocating the fourth slot; tp20_no_indication just withholds
    // the resulting indication so the wait loop never resolves on its own.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    // Give the physical channel's poll task time to notice the staleness on
    // its own next per-tick check and best-effort tear down the leaked slot
    // (mirrors this file's own precedent for a queued teardown's own
    // completion margin, e.g. `connection_establishes_and_tears_down_
    // cleanly`'s post-CoptStopcomm sleep).
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    server.backdoor.set_tp20_no_indication(false);

    // S4: a fourth sibling's own connection request -- succeeds (the freed
    // fourth slot) with the fix, or is rejected as a fifth-over-the-limit
    // request without it.
    let cll_s4 = create_and_connect_tp20_cll(&mut client, 0x0104, 5).await;
    let mut events_s4 = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_s4)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_s4)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events_s4, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "S4's own CoptStartcomm should finish (not hang), regardless of which side of the fix \
         is under test"
    );
    assert!(
        !saw_rsc_locked,
        "S4's own connection request should succeed once CLL A's own leaked slot is torn down \
         by the mid-loop staleness fix (best_effort_teardown_on_abandon) -- a PduErrEvtRscLocked \
         here means CLL A's slot was never freed, i.e. the mid-wait teardown fix did not run"
    );

    drop(events_s4);
    server.backdoor.set_tp20_no_indication(false);
    server.shutdown().await;
}

/// Codex review finding (PR #97, 6th round): `LogicalLinkState::tp20_
/// connection`'s own doc comment already promised a reset "at the next
/// ConnectComLogicalLink's finalization" (the same reconnect-only reasoning
/// `reconnecting_the_same_cll_handle_clears_the_stale_claimed_address`
/// (`j1939.rs`) proves for `j1939_claimed_address`), but nothing actually
/// implemented it until this fix (`rpc_link.rs`'s `finalize_connected_link`,
/// alongside the pre-existing SAE J1939 claim-state resets).
///
/// **Why the regression proof lives in `rpc_link.rs::tests` instead of
/// here, unlike every other reconnect-shaped test in this file:** the one
/// disconnect shape that actually leaves `tp20_connection` stale --
/// `events::handle_channel_hard_error`, NOT a clean `CoptStopcomm`/
/// `DisconnectComLogicalLink` (both already clear it unconditionally,
/// `handle_stop_comm`'s queued clear / `rpc_disconnect_com_logical_link`'s
/// own `take()`, so a clean-disconnect reconnect test the way `j1939.rs`'s
/// `reconnecting_the_same_cll_handle_clears_the_stale_claimed_address` does
/// for `j1939_claimed_address` would prove nothing new here) -- also
/// unconditionally marks the whole module `PduModstNotAvail`
/// (`handle_channel_hard_error`'s own unconditional `state.status =
/// PduModstNotAvail` at the end of the function). ADR-131/ADR-134 make that
/// status sticky until an explicit `ModuleDisconnect`, which this crate's own
/// `rpc_module_disconnect` (`rpc_module.rs`) implements by unconditionally
/// `links.clear()`-ing the ENTIRE `logical_links` map -- and
/// `rpc_connect_com_logical_link` itself rejects with `PDU_ERR_MODULE_NOT_
/// CONNECTED` for any not-yet-connected CLL while the module is `NotAvail`
/// (pinned by `rpc_link.rs::tests::connect_com_logical_link_rejects_when_
/// module_marked_not_avail`, confirmed empirically against this exact
/// scenario while implementing this test). So the SAME `cll_handle` a hard
/// error just took offline can only ever reconnect after a `ModuleDisconnect`
/// that has already destroyed its `LogicalLinkState` entry outright -- by the
/// time any `ConnectComLogicalLink` call can succeed again, the client has
/// necessarily gone through `CreateComLogicalLink` for a brand-new handle
/// whose `tp20_connection` was already `None` from `LogicalLinkState::
/// default()`, fix or no fix. The "hard error, then reconnect the same
/// handle, observe the stale `Established` state via `CoptSendrecv`" scenario
/// this fix's own doc comment and the originating Codex finding describe is
/// therefore not constructible through this crate's gRPC surface at all --
/// mirroring this file's own `non_established_sibling_does_not_disrupt_an_
/// established_peers_delivery` precedent for the identical class of harness
/// limit. The actual proof of `finalize_connected_link`'s own contribution is
/// `rpc_link.rs::tests::finalize_connected_link_resets_a_stale_tp20_
/// connection`, which calls `finalize_connected_link` directly on a
/// hand-built stale link and needs no mock hardware at all -- mirroring
/// `finalize_connected_link_resets_prior_sessions_error_suspension`'s
/// identical reasoning for the neighboring `tx_suspended_by_error` reset in
/// the very same critical section.
#[tokio::test]
#[serial]
async fn hard_error_then_reconnect_on_the_same_channel_is_reported_as_module_not_avail() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish (PduCopstFinished) once the connection establishes"
    );

    // Force a hard channel error (NOT a clean CoptStopcomm/Disconnect) --
    // `handle_channel_hard_error` takes the CLL offline without itself
    // clearing `tp20_connection`, reproducing the exact stale-state shape
    // `finalize_connected_link`'s reconnect-time reset is required to close.
    server.backdoor.set_read_msgs_error(Some(
        j2534_0404::ERR_DEVICE_NOT_CONNECTED as std::os::raw::c_long,
    ));
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == PduComLogicalLinkStatus::PduCllstOffline as i32
        ))
        .await,
        "expected the background poll task's hard-channel-error path to take this CLL offline"
    );
    drop(events);
    server.backdoor.set_read_msgs_error(None);

    // Pins the currently-correct, unaffected-by-this-fix behavior a naive
    // "just reconnect the same handle" attempt actually hits: rejected as
    // PDU_ERR_MODULE_NOT_CONNECTED (ADR-131/ADR-134), not a stale-state bug
    // -- so a future regression in EITHER direction (this gate silently
    // disappearing, or reconnect silently starting to succeed without a
    // ModuleDisconnect) is still caught here, even though it cannot exercise
    // `finalize_connected_link`'s own reset (see this test's own doc comment
    // for where that proof actually lives).
    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "reconnecting the same cll_handle directly, without an intervening \
             ModuleDisconnect+ModuleConnect, must still be rejected after a hard error",
        );
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    let detail = vci_service_interface::error_detail_from_status(&status)
        .expect("ErrorDetail should be attached");
    assert_eq!(
        detail.pdu_error,
        vci_service_interface::PduError::PduErrModuleNotConnected as i32
    );

    server.shutdown().await;
}

/// Codex review finding (P2, PR #97, 7th round -- ADR-188's own quarantine
/// mechanism): `run_tp20_connection_request`'s two local-abandonment paths
/// (mid-wait staleness, local-deadline timeout) both issue the native
/// `IOCTL_REQUEST_CONNECTION` successfully before giving up, so a delayed
/// `CONNECTION_ESTABLISHED`/`_LOST` indication can still arrive for `rx_id`
/// afterward. Before this fix, the routing entry was removed unconditionally
/// on abandonment, so an immediate retry proposing the SAME `rx_id` (from
/// the SAME `cll_handle`, still connected -- exactly the local-deadline
/// case, since no disconnect happened) registered a brand-new,
/// indistinguishable entry a later delayed indication could be misattributed
/// to. This test proves the actual observable half of that regression: the
/// retry itself must now be rejected while the abandoned attempt's own
/// quarantine is in effect, rather than silently succeeding (or hanging).
///
/// **Construction:** arms `__mock_set_tp20_no_indication` (this file's own
/// `disconnect_mid_wait_after_native_issue_tears_down_the_leaked_slot`
/// technique) so `IOCTL_REQUEST_CONNECTION` genuinely allocates a native
/// slot but never posts its indication, then lets the FIRST `CoptStartcomm`
/// run all the way out to its own ~2s local deadline (`Lost(1)`, the timeout
/// reason byte) rather than disconnecting mid-wait -- this is exactly the
/// local-deadline-abandonment scenario `docs/implementation-notes.md`'s
/// former Fix E test-coverage backlog entry named as unclosed (no test let
/// the armed wait run to its own local deadline without disconnecting), now
/// closed by this same test. The SECOND `CoptStartcomm`, on the SAME
/// `cll_handle`, proposing the SAME `CP_TP20RxIdProposal`, is issued
/// immediately after the first RPC call returns.
///
/// **Why `__mock_set_tp20_no_indication` stays armed through the retry too
/// (a mock-side extension needed to make this deterministic, discovered
/// while writing this test):** `best_effort_teardown_on_abandon`'s own
/// `IOCTL_TEARDOWN_CONNECTION` call, issued as PART of the first attempt's
/// own local-deadline abandonment, unconditionally removes the mock's slot
/// AND (before this test's own mock change) unconditionally queued its own
/// `CONNECTION_LOST` reason-`0` indication regardless of `tp20_no_
/// indication` -- and the physical channel's single poll task drains that
/// queued indication on the very next loop tick, structurally BEFORE it can
/// ever dequeue a subsequent client-driven RPC (`poll_channel_events`'s own
/// per-iteration `run_due_tick_duties` tail runs unconditionally right after
/// the first `CoptStartcomm`'s own dispatch returns, well before the retry
/// -- however promptly issued -- could ever be dequeued). That released the
/// quarantine so promptly that a same-`rx_id` retry could never observe it
/// still in effect, no matter how quickly it was issued -- the opposite of
/// "naturally deterministic," an assumption that turned out not to hold
/// once measured. `j2534-0404-mock`'s `IOCTL_TEARDOWN_CONNECTION` handler
/// now also gates that push behind `tp20_no_indication` (see its own field
/// doc) -- with it armed through both `CoptStartcomm` calls, the teardown
/// still genuinely frees the native slot (so a real device's own async-
/// teardown-confirmation race, the original finding's own scenario, has a
/// mock analogue at all), but the quarantine only actually releases once
/// something disarms the hook and a genuine indication is later drained --
/// which this test never needs to reach, since the retry is expected to be
/// rejected before ever registering a wait of its own.
///
/// Confirmed to catch the regression by temporarily commenting out the
/// `abandoned = true;` write at both `best_effort_teardown_on_abandon` call
/// sites in `run_tp20_connection_request` (`events_tp20_connection.rs`),
/// leaving the entry removed unconditionally exactly as before this fix,
/// and observing this test fail (the retry's own `CoptStartcomm` re-issues a
/// fresh native request and itself times out ~2s later instead of being
/// rejected immediately), then restoring it.
#[tokio::test]
#[serial]
async fn abandoned_connection_request_quarantines_its_rx_id_against_an_immediate_retry() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    server.backdoor.set_tp20_no_indication(true);

    // First attempt: the native call succeeds (the armed hook still
    // allocates the slot) but its indication is withheld, so this genuinely
    // runs the bounded wait out to its own ~2s local deadline (Lost(1))
    // rather than resolving early.
    let first_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    wait_for_cop_executing(&mut client, first_cop_handle).await;

    assert!(
        wait_for_event(&mut events, 3000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "the first CoptStartcomm should finish (Lost(1) via its own local-deadline \
         abandonment), not hang"
    );

    // Second attempt: the retry, on the SAME cll_handle, proposing the SAME
    // rx_id, issued right after the first RPC call above returned -- while
    // the first attempt's own quarantine should still be in effect.
    let cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously")
        .into_inner()
        .cop_handle;
    assert!(cop_handle.is_some(), "cop_handle should be present");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "the retry's own CoptStartcomm should finish (not hang), regardless of which side of \
         the fix is under test"
    );
    assert!(
        saw_rsc_locked,
        "an immediate retry proposing the same rx_id as an abandoned (never device-confirmed) \
         attempt must be rejected (PduErrEvtRscLocked / RxIdInUse) while that attempt's own \
         quarantine is still in effect -- seeing this succeed instead means the abandoned \
         entry's rx_id was not quarantined, exactly the misattribution risk this fix closes"
    );

    server.backdoor.set_tp20_no_indication(false);
    drop(events);
    server.shutdown().await;
}

/// Codex review finding (P2, PR #97, 8th round -- ADR-188's own quarantine
/// mechanism, Fix L): `deliver_tp20_connection_indication` used to resolve
/// its indication's owning CLL (`resolve_tp20_connection_indication`, a
/// LIVE-owner-generation-gated lookup) BEFORE ever checking whether the
/// matched entry was `abandoned` -- so once an abandoned entry's own
/// recorded owner CLL disappeared (disconnected/reconnected), that
/// resolution permanently returned `None` and the function returned early,
/// never reaching its own quarantine-release logic at all. The RX-ID then
/// stayed quarantined forever, for every CLL, until the whole physical
/// channel closed -- a self-inflicted permanent lock-out, worse than the
/// bug Fix J (7th round) closed. Fixed by checking `abandoned` FIRST,
/// independent of the entry's own owner liveness (see `deliver_tp20_
/// connection_indication`'s own doc comment for the fixed control flow).
///
/// **Construction:** reuses `abandoned_connection_request_quarantines_its_
/// rx_id_against_an_immediate_retry`'s own technique to abandon CLL A's own
/// `CoptStartcomm` via its local ~2s deadline (`__mock_set_tp20_no_
/// indication` armed throughout), quarantining `rx_id`. CLL A is then
/// DESTROYED outright (`DestroyComLogicalLink`, not merely
/// `DisconnectComLogicalLink` -- a plain disconnect leaves the CLL's own
/// `logical_links` entry in place with `connect_generation` unchanged, so
/// `resolve_tp20_connection_indication`'s `live_generation` lookup would
/// still resolve it and never reproduce this fix's own precondition;
/// destroying removes the entry entirely) -- its recorded `cll_handle` can
/// no longer resolve live at all, reproducing the exact "owner disappeared"
/// precondition this fix targets. The delayed indication itself (which a
/// real device could still deliver for the abandoned attempt, clause
/// 19.3.1) is synthesized directly via `inject_rx_with_status` -- the mock's
/// own `IOCTL_TEARDOWN_CONNECTION` handler only queues its `CONNECTION_LOST`
/// push when `tp20_no_indication` is DISARMED at call time (see that
/// handler's own doc comment in `j2534-0404-mock`), and it was armed
/// throughout CLL A's own teardown above, so no naturally-occurring
/// indication is available to wait for here; injecting one directly is the
/// same "simulate what the mock's own bookkeeping cannot produce a second
/// time for an already-freed slot" technique this file's own `j1939.rs`
/// sibling (`RX_FLAG_J1939_ADDRESS_LOST` spontaneous-loss injection) already
/// establishes for the structurally identical claim-indication routing.
/// Finally, a NEW CLL B proposes the SAME `rx_id` -- with the fix, the
/// quarantine was released by the injected indication and B establishes
/// normally; without the fix, B is rejected as still-quarantined, exactly
/// as CLL A's own retry was above, but now PERMANENTLY (every subsequent
/// attempt would also be rejected, not just an immediate one).
///
/// Confirmed to catch the regression by temporarily reordering `deliver_
/// tp20_connection_indication` back to resolving `cll_handle` via `resolve_
/// tp20_connection_indication` BEFORE the abandoned-entry check (the
/// pre-fix control flow) and observing this test fail (CLL B rejected with
/// `PduErrEvtRscLocked`, proving the quarantine never released), then
/// restoring the fix.
#[tokio::test]
#[serial]
async fn abandoned_entrys_quarantine_releases_even_after_its_owner_cll_is_gone() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    const RX_ID: u32 = 0x0321;

    // A sibling CLL that stays connected (never issuing its own
    // `CoptStartcomm`) for the whole test, purely to keep the physical
    // channel's `ref_count` above zero -- CLL A is disconnected below, and
    // if it were the SOLE occupant, that disconnect would tear down the
    // whole `SharedChannel` (including the very `tp20_connections`
    // quarantine entry this test exercises), the same "at least one other
    // module reference must remain" precondition `disconnect_mid_wait_
    // after_native_issue_tears_down_the_leaked_slot`'s own sibling CLLs
    // (S1-S3) establish above.
    let _cll_keepalive = create_and_connect_tp20_cll(&mut client, 0x0099, 0).await;

    let cll_a = create_and_connect_tp20_cll(&mut client, RX_ID, 1).await;
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    server.backdoor.set_tp20_no_indication(true);

    // CLL A's own attempt: runs out to its own ~2s local deadline (Lost(1)),
    // quarantining `rx_id` as `abandoned` (this file's own `abandoned_
    // connection_request_quarantines_its_rx_id_against_an_immediate_retry`
    // technique).
    let cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    wait_for_cop_executing(&mut client, cop_handle).await;
    assert!(
        wait_for_event(&mut events_a, 3000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's own CoptStartcomm should finish (Lost(1) via its own local-deadline \
         abandonment), not hang"
    );
    drop(events_a);

    // CLL A's owner disappears -- DESTROYED outright (not merely
    // disconnected: a plain `DisconnectComLogicalLink` leaves the CLL's own
    // `logical_links` entry in place with its `connect_generation`
    // unchanged, per `rpc_disconnect_com_logical_link`'s own "mark offline,
    // do not remove" shape -- `resolve_tp20_connection_indication`'s
    // `live_generation` lookup would then still resolve it, never
    // reproducing this fix's own precondition. `DestroyComLogicalLink`
    // removes the entry from `logical_links` entirely, so `live_generation`
    // genuinely returns `None` afterward -- the "owning CLL was destroyed"
    // case this module's own doc comment names).
    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("destroy_com_logical_link should succeed");
    assert_eq!(
        server.backdoor.disconnect_count(),
        0,
        "the physical channel must survive CLL A's own destroy -- this test needs the SAME \
         physical channel (and the SAME SharedChannel::tp20_connections quarantine entry) to \
         still be reachable by CLL B below, so at least one other module reference must remain; \
         since this module's only CLL just went away, a real leak here would indicate this \
         test's own setup is unintentionally tearing down the physical channel instead of \
         exercising the quarantine"
    );

    server.backdoor.set_tp20_no_indication(false);

    // Synthesize the delayed CONNECTION_LOST indication a real device could
    // still deliver for CLL A's own abandoned attempt (clause 19.3.1) --
    // `Data[0..3]` (MSB first) = rx_id, `Data[4]` = reason (0, matching the
    // real teardown-generated indication's own reason byte).
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x00, 0x00, 0x03, 0x21, 0x00],
        j2534_0404::PROTOCOL_TP2_0_PS,
        0x0002_0000, // RX_TP20_CONNECTION_LOST, events.rs::RX_TP20_CONNECTION_LOST
    );

    // No client-visible event marks the quarantine's own release (it is not
    // COP-driven) -- give the channel poll task's periodic tick several
    // iterations (POLL_INTERVAL_MS = 10ms) to drain the injected frame,
    // mirroring `j1939.rs`'s own spontaneous-loss injection margin.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // CLL B: a brand-new CLL proposing the SAME rx_id. With the fix, the
    // quarantine was released by the indication above and B establishes
    // normally; without the fix, `rx_id` stays permanently quarantined and
    // B is rejected.
    let cll_b = create_and_connect_tp20_cll(&mut client, RX_ID, 2).await;
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events_b, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "CLL B's own CoptStartcomm should finish (not hang), regardless of which side of the \
         fix is under test"
    );
    assert!(
        !saw_rsc_locked,
        "CLL B must be able to establish on rx_id once the abandoned entry's own quarantine is \
         released by its delayed indication -- seeing PduErrEvtRscLocked here means the \
         quarantine never released after CLL A (the abandoned entry's own recorded owner) \
         disconnected, exactly the permanent-lock-out regression this fix closes"
    );

    drop(events_b);
    server.backdoor.set_tp20_no_indication(false);
    server.shutdown().await;
}

/// Codex review finding (P2, PR #97, 14th round): `IOCTL_TEARDOWN_CONNECTION`
/// is non-blocking, so `handle_stop_comm`'s own best-effort teardown call can
/// return well before the device's delayed `CONNECTION_LOST` confirmation
/// actually arrives. Before this fix, `handle_stop_comm` never registered any
/// quarantine entry for the torn-down `rx_id` at all (unlike the LOCAL-
/// abandonment paths, Fix J/K), so nothing prevented a promptly-issued new
/// `CoptStartcomm` proposing the SAME `rx_id` from registering its own
/// pending entry before that stale confirmation drained -- risking
/// misattribution of the old teardown's own delayed indication onto the new
/// request. This test proves the actual observable half of that regression,
/// the same way this file's own `abandoned_connection_request_quarantines_
/// its_rx_id_against_an_immediate_retry` (7th round) does for the
/// local-abandonment paths: the retry itself must now be rejected while the
/// just-torn-down connection's own quarantine is in effect, rather than
/// racing.
///
/// **Construction:** CLL A establishes normally first (indication NOT
/// withheld, so this differs from the local-abandonment test's own
/// from-the-start-armed setup). `__mock_set_tp20_no_indication` is armed
/// only AFTER establishing, immediately before `CoptStopcomm` -- per that
/// backdoor's own extended field doc (Fix J, 7th round), this also gates
/// `IOCTL_TEARDOWN_CONNECTION`'s own `CONNECTION_LOST` push, so the teardown
/// call genuinely frees the mock's real slot (a real device's own
/// async-teardown-confirmation race has a mock analogue at all) while
/// withholding its own indication -- keeping the quarantine open long enough
/// for the immediate retry below to observe it deterministically, mirroring
/// the 7th-round test's own identical reasoning for why the toggle must stay
/// armed through the retry.
///
/// Confirmed to catch the regression by temporarily reverting the
/// `quarantine_tp20_connection_for_orphaned_write_back` call this fix adds to
/// `handle_stop_comm` (leaving the native teardown call itself intact) and
/// observing this test fail (the retry re-issues a fresh native request
/// instead of being rejected immediately), then restoring it.
#[tokio::test]
#[serial]
async fn stopcomm_quarantines_its_rx_id_against_an_immediate_retry() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's own connection should establish first"
    );

    // Armed only now, right before StopComm -- also gates the teardown's own
    // indication push (Fix J's own extension), so the native call still
    // genuinely frees the mock's slot while withholding confirmation.
    server.backdoor.set_tp20_no_indication(true);

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
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStopcomm should finish"
    );

    // The retry: same cll_handle, same rx_id, issued right after StopComm's
    // own RPC call returned -- while its quarantine should still be in
    // effect.
    let cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously")
        .into_inner()
        .cop_handle;
    assert!(cop_handle.is_some(), "cop_handle should be present");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "the retry's own CoptStartcomm should finish (not hang), regardless of which side of \
         the fix is under test"
    );
    assert!(
        saw_rsc_locked,
        "an immediate retry proposing the same rx_id as a just-torn-down (not yet \
         device-confirmed) connection must be rejected (PduErrEvtRscLocked / RxIdInUse) while \
         its own quarantine is still in effect -- seeing this succeed instead means \
         handle_stop_comm never quarantined the rx_id, exactly the misattribution risk this \
         fix closes"
    );

    server.backdoor.set_tp20_no_indication(false);
    drop(events);
    server.shutdown().await;
}

/// Codex review finding (P2, PR #97, 15th round): Fix O's own re-teardown
/// on a delayed `Established` outcome (11th round) released the abandoned
/// entry's quarantine BEFORE issuing its own follow-up `IOCTL_
/// TEARDOWN_CONNECTION` call -- but that follow-up call is itself just as
/// non-blocking as any other teardown (Fix R's own finding, 14th round), so
/// a promptly-issued new `CoptStartcomm` proposing the SAME `rx_id` could
/// register before the follow-up's own delayed `CONNECTION_LOST`
/// confirmation drains, reproducing the identical misattribution risk Fix R
/// closed for the normal-teardown paths. This test proves the entry now
/// stays quarantined THROUGH the `Established` outcome's own processing,
/// releasing only once a SUBSEQUENT indication (the follow-up teardown's
/// own eventual confirmation) arrives.
///
/// **Construction:** CLL A's own attempt abandons via its local ~2s
/// deadline (`Lost(1)`, this file's own `abandoned_connection_request_
/// quarantines_its_rx_id_against_an_immediate_retry` technique),
/// quarantining `rx_id`. A synthetic `CONNECTION_ESTABLISHED` indication for
/// `rx_id` is then injected directly (`inject_rx_with_status`, the same
/// "simulate what the mock's own bookkeeping cannot reproduce a second
/// time" technique `abandoned_entrys_quarantine_releases_even_after_its_
/// owner_cll_is_gone` (8th round) already establishes) -- the mock's own
/// real `tp20_connections` slot for `rx_id` was already freed by the FIRST
/// teardown call at abandonment time, so this indication stands in for the
/// device's own genuinely-delayed confirmation a real device could still
/// deliver. A NEW CLL B then proposes the SAME `rx_id`: with the fix, the
/// entry never released on the `Established` outcome, so B is rejected as
/// still-quarantined; a second synthetic `CONNECTION_LOST` indication then
/// simulates the follow-up teardown's own eventual confirmation, releasing
/// the quarantine -- a THIRD attempt (CLL C) then establishes normally.
///
/// Confirmed to catch the regression by temporarily reverting the
/// `is_established` guard this fix adds to `deliver_tp20_connection_
/// indication`'s abandoned-entry branch (removing the entry unconditionally
/// on release, exactly as before this fix) and observing this test fail
/// (CLL B establishes instead of being rejected), then restoring it.
#[tokio::test]
#[serial]
async fn abandoned_entrys_established_outcome_stays_quarantined_until_the_followup_teardown_confirms()
 {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    const RX_ID: u32 = 0x0321;

    let cll_a = create_and_connect_tp20_cll(&mut client, RX_ID, 1).await;
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    server.backdoor.set_tp20_no_indication(true);

    // CLL A's own attempt: runs out to its own ~2s local deadline (Lost(1)),
    // quarantining `rx_id` as `abandoned`.
    let cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously")
        .into_inner()
        .cop_handle
        .expect("cop_handle should be present");
    wait_for_cop_executing(&mut client, cop_handle).await;
    assert!(
        wait_for_event(&mut events_a, 3000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's own CoptStartcomm should finish (Lost(1) via its own local-deadline \
         abandonment), not hang"
    );
    drop(events_a);

    server.backdoor.set_tp20_no_indication(false);

    // Synthesize the delayed CONNECTION_ESTABLISHED indication a real
    // device could still deliver for CLL A's own abandoned attempt --
    // `Data[0..3]` (MSB first) = rx_id, `Data[4..7]` = a mock-assigned TX-ID.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x00, 0x00, 0x03, 0x21, 0x10, 0x00, 0x03, 0x21],
        j2534_0404::PROTOCOL_TP2_0_PS,
        0x0001_0000, // RX_TP20_CONNECTION_ESTABLISHED, events.rs::RX_TP20_CONNECTION_ESTABLISHED
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // CLL B: proposes the SAME rx_id right after the Established outcome was
    // processed. With the fix, the entry never released on that outcome --
    // B is rejected as still-quarantined.
    let cll_b = create_and_connect_tp20_cll(&mut client, RX_ID, 2).await;
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events_b, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "CLL B's own CoptStartcomm should finish (not hang)"
    );
    assert!(
        saw_rsc_locked,
        "CLL B must be rejected (PduErrEvtRscLocked / RxIdInUse) right after the abandoned \
         entry's own Established outcome was processed -- seeing this succeed means the \
         quarantine was released before the follow-up teardown's own confirmation could drain, \
         exactly the misattribution risk this fix closes"
    );
    drop(events_b);

    // Synthesize the follow-up teardown's own eventual CONNECTION_LOST
    // confirmation, finally releasing the quarantine.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x00, 0x00, 0x03, 0x21, 0x00],
        j2534_0404::PROTOCOL_TP2_0_PS,
        0x0002_0000, // RX_TP20_CONNECTION_LOST, events.rs::RX_TP20_CONNECTION_LOST
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // CLL C: now succeeds once the quarantine has genuinely released.
    let cll_c = create_and_connect_tp20_cll(&mut client, RX_ID, 3).await;
    let mut events_c = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_c)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_c)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished_c = false;
    let mut saw_rsc_locked_c = false;
    wait_for_event(&mut events_c, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked_c = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished_c = true;
            }
            _ => {}
        }
        finished_c
    })
    .await;
    assert!(
        finished_c,
        "CLL C's own CoptStartcomm should finish (not hang)"
    );
    assert!(
        !saw_rsc_locked_c,
        "CLL C should establish normally once the quarantine has genuinely released by the \
         follow-up teardown's own confirmation"
    );

    drop(events_c);
    server.shutdown().await;
}

/// Codex review regression (P1, PR #97, round 22; design-advisor consult):
/// a TP2.0 connection spontaneously lost by the device AFTER establishment
/// (no local abandonment involved at all) used to be silently dropped --
/// `run_tp20_connection_request`'s own post-loop cleanup already removed
/// this rx_id's `tp20_connections` routing entry once establishment
/// succeeded, so the unsolicited `CONNECTION_LOST` indication had no entry
/// to resolve against and the owning CLL's own `tp20_connection.phase`
/// stayed `Established` forever (ADR-188's own accepted "no ongoing
/// monitoring for a later spontaneous loss" residual). A later
/// `CoptStopcomm` on that CLL then believed the connection was still live,
/// issued a doomed native teardown, and (since round 21's unconditional
/// quarantine) inserted a quarantine entry NOTHING would ever release --
/// the `Lost` confirmation for this connection had already arrived and been
/// discarded before that quarantine entry ever existed: a permanent
/// lock-out. Fixed by `deliver_tp20_connection_indication`'s new
/// reconciliation arm (`reconcile_established_tp20_loss`): an unmatched
/// `Lost` indication now flips the owning CLL's own `tp20_connection.phase`
/// to `Lost` directly, so `CoptStopcomm` sees a connection that's already
/// gone and skips the native teardown (and the quarantine-insert) entirely.
#[tokio::test]
#[serial]
async fn spontaneous_loss_after_establishment_reconciles_before_a_later_stopcomm() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    const RX_ID: u32 = 0x0321;

    let cll_a = create_and_connect_tp20_cll(&mut client, RX_ID, 1).await;
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's own CoptStartcomm should finish once the connection establishes"
    );
    drop(events_a);

    // Synthesize the device spontaneously losing the connection AFTER
    // establishment -- clause 19.3.1's own maintenance-timeout expiry, a
    // cable pull, or any other native-side loss unrelated to any teardown
    // request from this service. `Data[0..3]` (MSB first) = rx_id,
    // `Data[4]` = reason (0).
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x00, 0x00, 0x03, 0x21, 0x00],
        j2534_0404::PROTOCOL_TP2_0_PS,
        0x0002_0000, // RX_TP20_CONNECTION_LOST, events.rs::RX_TP20_CONNECTION_LOST
    );
    // No client-visible event marks the reconciliation itself -- give the
    // channel poll task several ticks to drain the injected frame and run
    // `reconcile_established_tp20_loss` (POLL_INTERVAL_MS = 10ms).
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // CLL A's own CoptStopcomm: with the fix, `tp20_connection.phase` is
    // already `Lost`, so this issues no native teardown and inserts no
    // quarantine at all -- must finish cleanly regardless.
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's own CoptStopcomm should finish cleanly (not hang) even though the connection \
         was already spontaneously lost before this ran"
    );
    drop(events_a);

    // CLL B: a brand-new CLL proposing the SAME rx_id, on the SAME still-
    // open physical channel (CLL A stays connected). Without the fix, this
    // is rejected forever (PduErrEvtRscLocked / RxIdInUse) -- the quarantine
    // CLL A's own CoptStopcomm inserted above has no confirmation left to
    // ever release it, since the real Lost confirmation already arrived and
    // was consumed before that quarantine entry ever existed.
    let cll_b = create_and_connect_tp20_cll(&mut client, RX_ID, 2).await;
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events_b, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "CLL B's own CoptStartcomm should finish (not hang)"
    );
    assert!(
        !saw_rsc_locked,
        "CLL B must establish normally on the same rx_id -- a rejection here means the \
         spontaneous-loss-after-establishment reconciliation fix regressed, permanently \
         quarantining rx_id with no confirmation left to ever release it"
    );

    drop(events_b);
    server.shutdown().await;
}

/// Codex review regression (P1, PR #97, round 24; found by `edge-case-
/// hunter`, fixed per `design-advisor` consult): a stale spontaneous-loss
/// `CONNECTION_LOST` indication for an already-cleanly-resolved TP2.0
/// connection (CLL A, established then later independently lost by the
/// device -- NOT abandoned, NOT quarantined) must never misattribute onto
/// an unrelated, brand-new `CoptStartcomm` (CLL B) that legitimately
/// reuses the SAME `rx_id` while A's stale indication is still undrained.
/// Before this fix, `deliver_tp20_connection_indication`'s routing was
/// keyed only by whichever entry CURRENTLY occupies `rx_id` in
/// `SharedChannel::tp20_connections` -- with nothing to tell "the CURRENT
/// occupant" apart from "the specific request attempt an old, delayed
/// indication actually belongs to" once A's own entry had already been
/// cleanly removed (the ordinary "resolves either way" cleanup) and B had
/// registered a fresh, unrelated entry under the same key. B's own
/// `CoptStartcomm` would then fail with A's stale `Lost` reason, and B's
/// OWN genuine `CONNECTION_ESTABLISHED` (if its real native request also
/// succeeded) would find no entry left to resolve against -- a spurious
/// COP failure plus a silently leaked native connection slot.
///
/// `tp20_teardown_connection_directly` (the harness's own raw-IOCTL
/// backdoor, bypassing this service's tracking entirely) simulates the
/// device spontaneously losing A's connection -- freeing the mock's own
/// `rx_id` slot and auto-queuing a `CONNECTION_LOST` indication -- for a
/// reason this service was never told about, exactly the scenario clause
/// 19.3.3.2's uniqueness rule lets B's own subsequent `IOCTL_REQUEST_
/// CONNECTION` for the SAME `rx_id` succeed against. With the fix,
/// `run_tp20_connection_request`'s own registration-time reconcile
/// (triggered the instant B's native request succeeds, proving A's own
/// `Established` belief is already stale device-side) flips A's own
/// `tp20_connection.phase` to `Lost` -- via whichever internal path
/// actually reconciles it first: either arming B's own entry with
/// `expect_stale_lost` so A's stale indication is later swallowed, or (if
/// the poll task drains A's stale indication before B's own registration
/// gets a chance to arm the flag -- empirically the path this test's own
/// timing actually exercises, round 25, PR #97) the round-22 no-match
/// fallback (`reconcile_established_tp20_loss`) reconciling A directly.
/// This test asserts only the OBSERVABLE outcome -- B establishes cleanly
/// and A's own state is correctly reconciled -- not which internal path
/// got there; the `expect_stale_lost` one-shot swallow property itself is
/// proven deterministically instead by `tp20_connection_indication_is_a_
/// swallowed_stale_lost`'s own unit tests
/// (`events_tp20_connection.rs`'s `#[cfg(test)]` module).
#[tokio::test]
#[serial]
async fn stale_spontaneous_loss_never_misattributes_onto_an_unrelated_new_startcomm() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    const RX_ID: u32 = 0x0321;

    // CLL A: establishes completely normally -- no abandonment, no
    // quarantine involved anywhere in this test.
    let cll_a = create_and_connect_tp20_cll(&mut client, RX_ID, 1).await;
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events_a, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's own CoptStartcomm should finish once the connection establishes"
    );
    drop(events_a);

    // Simulate the device spontaneously losing A's connection: frees the
    // mock's own rx_id slot and auto-queues CONNECTION_LOST(reason=0) --
    // entirely bypassing this service, which is never told a teardown was
    // requested at all.
    assert!(
        server
            .backdoor
            .tp20_teardown_connection_directly(MOCK_CHANNEL_ID, RX_ID),
        "the raw teardown IOCTL against the mock should succeed"
    );

    // CLL B: a brand-new CLL proposing the SAME rx_id, registered
    // immediately -- no sleep here on purpose. Clause 19.3.3.2's own
    // uniqueness rule means B's native IOCTL_REQUEST_CONNECTION can only
    // succeed because the mock's slot was just freed above, which is
    // exactly the proof the registration-time reconcile relies on; the
    // fix's own correctness does not depend on any particular interleaving
    // with the poll task's own drain of A's still-queued stale indication.
    let cll_b = create_and_connect_tp20_cll(&mut client, RX_ID, 2).await;
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished_b = false;
    let mut saw_error_b = false;
    wait_for_event(&mut events_b, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(_)) => {
                saw_error_b = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished_b = true;
            }
            _ => {}
        }
        finished_b
    })
    .await;
    assert!(
        finished_b,
        "CLL B's own CoptStartcomm should finish (not hang)"
    );
    assert!(
        !saw_error_b,
        "CLL B's own CoptStartcomm must establish normally -- any error here means A's stale \
         spontaneous-loss reason was misattributed onto B's own, unrelated request"
    );
    drop(events_b);

    // Direct proof B's own connection is genuinely, correctly established
    // (not corrupted): its own real assigned TX-ID prefixes a CoptSendrecv.
    send_data(&mut client, cll_b, vec![0xAA, 0xBB, 0xCC], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written,
        vec![0x10, 0x00, 0x03, 0x21, 0xAA, 0xBB, 0xCC],
        "CLL B's own established TX-ID should prefix its CoptSendrecv payload, proving its \
         connection is genuinely Established and not left in some corrupted intermediate state"
    );

    // Direct proof A's own state was correctly reconciled to Lost (not
    // left stuck at Established): a send on A must now be rejected the
    // same way an unestablished TP2.0 CLL's send always is.
    let send_result = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_a),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0xAA],
            cop_ctrl_data: None,
        })
        .await;
    assert!(
        send_result.is_err(),
        "CLL A's own CoptSendrecv must be rejected once its connection has been reconciled to \
         Lost -- succeeding here means A was never actually reconciled"
    );

    server.shutdown().await;
}

/// Codex review finding on Fix AA itself (P1, round 25, PR #97;
/// design-advisor consult continuing the round-24 escalation): does the
/// `expect_stale_lost` one-shot swallow risk eating a genuinely NEW `Lost`
/// CLL B's own request produces, mistaking it for CLL A's stale one, since
/// Table 81's payload carries no per-attempt correlation token? Confirmed
/// safe on a conformant device (see ADR-188's Fix AA bullet, Consequences,
/// for the full clause-7.2.5 ordering argument): the device can only ever
/// queue A's own stale `CONNECTION_LOST` strictly BEFORE any outcome B's
/// own later registration can produce, since B's registration can only
/// succeed once the device already recorded A's own loss (the same
/// clause-19.3.3.2 proof this whole mechanism already rests on).
///
/// CLL A establishes normally on `RX_ID`, alongside three filler CLLs that
/// occupy this physical channel's other three
/// `TP20_MAX_CONNECTIONS_PER_CHANNEL` slots (mirrors
/// `connection_rejected_when_all_four_slots_are_full`'s own setup). A's
/// connection is then spontaneously lost via the raw teardown backdoor
/// (frees A's own mock-side slot, queues A's stale `CONNECTION_LOST`
/// reason `0` FIRST). A fourth filler immediately re-fills the freed slot,
/// restoring the channel to full four-slot capacity BEFORE CLL B's own
/// registration -- so when B proposes the SAME `rx_id` A just vacated, B's
/// own native `IOCTL_REQUEST_CONNECTION` itself hits the "all four slots
/// full" resource-exhaustion path (clause 19.3.3.2) and queues B's OWN
/// `CONNECTION_LOST` reason `0xD8` SECOND, strictly after A's already-
/// queued stale one.
///
/// This asserts the OBSERVABLE end-to-end invariant only: B must surface
/// its OWN reason (`0xD8` -> `PduErrEvtRscLocked`), never A's stale one
/// (`0` -> `PduErrEvtInitError`). It deliberately does NOT pin, and cannot
/// reliably pin, WHICH of two correct internal paths delivers that result
/// -- the registration-time `expect_stale_lost` swallow (this bullet's own
/// mechanism), or the round-22 no-match fallback
/// (`reconcile_established_tp20_loss`) reconciling A first if the mock's
/// own RX-poll task happens to drain A's queued frame before B's gRPC
/// registration dispatches. Empirically, the fallback wins this race
/// systematically in this harness (client-driven dispatch latency reliably
/// exceeds the poll interval), not the other way round -- so treat this
/// test as end-to-end regression coverage for the user-visible guarantee,
/// not as proof the swallow branch itself executed. That specific,
/// flag-state-controlled property (a second, later `Lost` is never
/// swallowed twice) is proven deterministically instead by
/// `tp20_connection_indication_is_a_swallowed_stale_lost`'s own unit tests
/// in `events_tp20_connection.rs`'s `#[cfg(test)]` module.
#[tokio::test]
#[serial]
async fn stale_lost_swallow_is_one_shot_and_never_eats_the_new_occupants_own_genuine_loss() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    const RX_ID: u32 = 0x0321;

    async fn establish(
        client: &mut vci_service_interface::vci_service_client::VciServiceClient<
            tonic::transport::Channel,
        >,
        cll_handle: ComLogicalLinkHandle,
        fail_msg: &str,
    ) {
        let mut events = client
            .subscribe_event(vci_service_interface::SubscribeEventRequest {
                handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("subscribe_event should succeed")
            .into_inner();
        start_comm(client, cll_handle)
            .await
            .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
        assert!(
            wait_for_event(&mut events, 2000, |item| matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ))
            .await,
            "{fail_msg}"
        );
    }

    // CLL A plus three fillers occupy this physical channel's full
    // TP20_MAX_CONNECTIONS_PER_CHANNEL capacity (mirrors
    // `connection_rejected_when_all_four_slots_are_full`'s own setup).
    let cll_a = create_and_connect_tp20_cll(&mut client, RX_ID, 1).await;
    let mut filler_handles = Vec::new();
    for (i, rx_id) in [0x0400u32, 0x0401, 0x0402].into_iter().enumerate() {
        filler_handles.push(create_and_connect_tp20_cll(&mut client, rx_id, i as u64 + 2).await);
    }
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "CLL A and all fillers should share one physical channel"
    );

    establish(
        &mut client,
        cll_a,
        "CLL A's own CoptStartcomm should finish once the connection establishes",
    )
    .await;
    for &filler in &filler_handles {
        establish(
            &mut client,
            filler,
            "each filler CLL should establish, filling this channel's four-slot capacity",
        )
        .await;
    }

    // Simulate the device spontaneously losing A's connection: frees the
    // mock's own rx_id slot (dropping this channel to three occupied
    // slots) and auto-queues CONNECTION_LOST(reason=0) FIRST.
    assert!(
        server
            .backdoor
            .tp20_teardown_connection_directly(MOCK_CHANNEL_ID, RX_ID),
        "the raw teardown IOCTL against the mock should succeed"
    );

    // A fourth filler immediately re-fills the freed slot (a new, distinct
    // rx_id -- never RX_ID), restoring this channel to full four-slot
    // capacity BEFORE CLL B's own registration below, so B's own native
    // request lands on a genuinely full channel.
    let filler4 = create_and_connect_tp20_cll(&mut client, 0x0403, 5).await;
    establish(
        &mut client,
        filler4,
        "the fourth filler should establish normally, refilling the channel to capacity",
    )
    .await;

    // CLL B: proposes the SAME rx_id A just vacated, registered immediately
    // -- no sleep on purpose (mirrors the round-24 test's own reasoning:
    // clause 7.2.5's own read-in-event-order guarantee, not any particular
    // poll-task interleaving, is what fixes the two queued frames' order).
    // With the channel now genuinely full, B's own native
    // IOCTL_REQUEST_CONNECTION hits the resource-exhaustion path and
    // queues B's OWN CONNECTION_LOST(reason=0xD8) SECOND, after A's
    // already-queued stale one.
    let cll_b = create_and_connect_tp20_cll(&mut client, RX_ID, 6).await;
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished_b = false;
    let mut saw_rsc_locked_b = false;
    let mut saw_init_error_b = false;
    wait_for_event(&mut events_b, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked_b = true;
            }
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtInitError as i32 =>
            {
                saw_init_error_b = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished_b = true;
            }
            _ => {}
        }
        finished_b
    })
    .await;
    assert!(
        finished_b,
        "CLL B's own CoptStartcomm should finish (not hang waiting past its own genuine outcome)"
    );
    assert!(
        saw_rsc_locked_b,
        "CLL B must surface its OWN reason (0xD8 -> PduErrEvtRscLocked), via whichever internal \
         reconciliation path actually ran for A's stale frame -- not A's stale reason"
    );
    assert!(
        !saw_init_error_b,
        "CLL B must never surface A's stale reason (0 -> PduErrEvtInitError) -- seeing this would \
         mean A's stale Lost was misattributed onto B's own, unrelated request, exactly the \
         misattribution Codex's round-25 finding raised"
    );
    drop(events_b);

    server.shutdown().await;
}

// ══════════════════════════════════════════════════════════════════════════
// ADR-190 (Phase 7 Stage 7b): TP2.0 passive connections
// ══════════════════════════════════════════════════════════════════════════

/// Item 1 (ADR-190 Decision, "arm-and-complete"): staging both
/// `CP_TP20PassiveIdentifier`/`CP_TP20PassiveRxId` then `CoptStartcomm`
/// completes IMMEDIATELY (`PduCopstFinished`, no error event) -- it must not
/// wait for an inbound connection at all. Establishment is then proven
/// indirectly (ADR-190 Consequences: no client-visible establish event) via
/// the mock's own `__mock_inject_tp20_passive_connection` backdoor
/// simulating the network-side accept, followed by a successful
/// `CoptSendrecv` whose wire framing carries the peer-assigned TX-ID --
/// proving TX reuses Stage 7a's mechanism verbatim once established (ADR-190
/// section 3).
#[tokio::test]
#[serial]
async fn passive_listener_arms_immediately_then_establishes_and_exchanges_data() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "arm-and-complete: CoptStartcomm should finish immediately once armed, never waiting for \
         an inbound connection (ADR-190 Decision)"
    );
    drop(events);

    assert!(
        server
            .backdoor
            .inject_tp20_passive_connection(MOCK_CHANNEL_ID, 0x1000_0350),
        "the injection should land -- the listener is armed and the channel is not full"
    );
    // No client-visible establish event exists (ADR-190 Consequences) --
    // give the poll task a few ticks to drain the injected indication
    // (POLL_INTERVAL_MS = 10ms).
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    send_data(&mut client, cll_handle, vec![0xAA, 0xBB, 0xCC], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        written,
        vec![0x10, 0x00, 0x03, 0x50, 0xAA, 0xBB, 0xCC],
        "the peer-assigned TX-ID (0x1000_0350) should prefix the CoptSendrecv payload, proving \
         the passive connection reached Established"
    );

    server.shutdown().await;
}

/// Codex review finding (P2, PR #99, round 4): `arm_tp20_passive_listener`
/// used to write `TP2_0_IDENTIFER` then `TP2_0_RXIDPASSIVE`, which is
/// unsafe whenever a prior disarm left a stale nonzero `TP2_0_RXIDPASSIVE`
/// on the device (its own best-effort clear can fail independently per
/// param) -- writing the new identifier first would briefly pair it with
/// that stale rx_id, satisfying clause 19.3.3.1's both-nonzero accept
/// condition for an rx_id with no registered routing entry. Fixed with a
/// defensive `TP2_0_IDENTIFER=0` write first (neutralizing either
/// direction of stale residual), then the new `TP2_0_RXIDPASSIVE`, then the
/// new `TP2_0_IDENTIFER` last (the single call that actually enables the
/// listener, by which point `TP2_0_RXIDPASSIVE` already holds its correct
/// value). This test cannot construct the actual native race (no mock
/// hook injects a selective per-call `SET_CONFIG` failure, the same
/// accepted-residual class this file already documents for the arm's own
/// second-call-failure rollback path), but proves the call SEQUENCE the
/// fix depends on is exactly what ADR-190's Correction paragraph describes,
/// and that the final native config values are correct once armed.
#[tokio::test]
#[serial]
async fn passive_arm_issues_a_defensive_identifer_zero_before_the_final_enable() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 1).await;
    let log_len_before_arm = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID).len();

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await
    );
    drop(events);

    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    let arm_log = &log[log_len_before_arm..];
    assert_eq!(
        arm_log,
        &[
            j2534_0404::CONFIG_TP2_0_IDENTIFER,
            j2534_0404::CONFIG_TP2_0_RXIDPASSIVE,
            j2534_0404::CONFIG_TP2_0_IDENTIFER,
        ],
        "arming must write TP2_0_IDENTIFER=0 defensively first, then the new TP2_0_RXIDPASSIVE, \
         then the new TP2_0_IDENTIFER last -- any other order can pair a stale native value \
         with a live nonzero counterpart; log was {arm_log:#x?}"
    );

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_TP2_0_IDENTIFER),
        0x0250,
        "the final call in the sequence should leave TP2_0_IDENTIFER at its intended value"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_TP2_0_RXIDPASSIVE),
        0x0350,
        "TP2_0_RXIDPASSIVE should hold its intended value once armed"
    );

    server.shutdown().await;
}

/// Item 2 (ADR-190 section 2): a second CLL's own passive-arm attempt is
/// rejected while the interface's single passive slot is already armed by
/// another CLL -- even proposing a completely different identifier/
/// rx_id_passive pair, and even though both CLLs share the same physical
/// channel.
#[tokio::test]
#[serial]
async fn second_cll_passive_arm_is_rejected_while_one_is_already_armed() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 1).await;
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events_a, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's own passive arm should complete immediately"
    );
    drop(events_a);

    let cll_b = create_and_connect_tp20_cll_passive(&mut client, 0x0260, 0x0360, 2).await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "both CLLs should share one physical channel (same resource id/pins)"
    );
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events_b, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "CLL B's own CoptStartcomm should finish (not hang or get cancelled)"
    );
    assert!(
        saw_rsc_locked,
        "CLL B's passive-arm attempt should be rejected with PduErrEvtRscLocked -- the interface \
         has only one passive slot (ADR-190 section 2)"
    );
    drop(events_b);

    server.shutdown().await;
}

/// Item 3a (ADR-190 section 3 step 3): an active connection already
/// `Established` on `rx_id` blocks a DIFFERENT CLL's passive-arm attempt
/// proposing that same value as `rx_id_passive` -- the active connection's
/// own routing-map entry is removed once it establishes, so the ordinary
/// `tp20_rx_id_unavailable_for` map check alone cannot see it; this is the
/// dedicated live-CLL scan step 3 adds.
#[tokio::test]
#[serial]
async fn active_established_connection_blocks_a_passive_arm_on_the_same_rx_id() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_active = create_and_connect_tp20_cll(&mut client, 0x0350, 1).await;
    let mut events_active = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_active)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_active)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events_active, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "the active connection should establish"
    );
    drop(events_active);

    let cll_passive = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 2).await;
    assert_eq!(server.backdoor.connect_count(), 1);
    let mut events_passive = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_passive)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_passive)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events_passive, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(finished, "the passive-arm attempt should finish (not hang)");
    assert!(
        saw_rsc_locked,
        "an active-Established connection on rx_id 0x0350 should block a passive arm proposing \
         the same rx_id_passive (ADR-190 section 3 step 3)"
    );
    drop(events_passive);

    server.shutdown().await;
}

/// Item 3b (the reverse direction, already covered by the existing
/// `tp20_rx_id_unavailable_for` check per ADR-190 section 3 step 2, but not
/// previously regression-tested end-to-end for a passive occupant): an
/// already-armed passive slot's own persistent routing entry blocks a
/// DIFFERENT CLL's active `CoptStartcomm` proposing the same rx_id.
#[tokio::test]
#[serial]
async fn passive_armed_slot_blocks_an_active_proposal_on_the_same_rx_id() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_passive = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 1).await;
    let mut events_passive = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_passive)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_passive)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events_passive, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "the passive arm should complete immediately"
    );
    drop(events_passive);

    let cll_active = create_and_connect_tp20_cll(&mut client, 0x0350, 2).await;
    assert_eq!(server.backdoor.connect_count(), 1);
    let mut events_active = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_active)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_active)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events_active, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "the active connection attempt should finish (not hang)"
    );
    assert!(
        saw_rsc_locked,
        "an already-armed passive slot's own persistent routing entry for rx_id 0x0350 should \
         block a new active CoptStartcomm proposing the same rx_id (existing tp20_rx_id_\
         unavailable_for check, ADR-190 section 3 step 2)"
    );
    drop(events_active);

    server.shutdown().await;
}

/// Item 4 (ADR-190 section 3, the `Listening` re-entry): a passive
/// connection that establishes, spontaneously loses (device-side, e.g. the
/// maintenance timeout), then RE-establishes on a fresh inbound accept for
/// the SAME `rx_id_passive` -- proving the persistent `Tp20ConnEntry`
/// survives the loss (never removed, unlike an active connection) and the
/// CLL's own phase re-enters `Listening` (never a terminal `Lost`), so the
/// device's next auto-accepted connection routes correctly.
#[tokio::test]
#[serial]
async fn passive_connection_re_listens_after_a_spontaneous_loss_and_re_establishes() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await
    );
    drop(events);

    assert!(
        server
            .backdoor
            .inject_tp20_passive_connection(MOCK_CHANNEL_ID, 0x1000_0350)
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    send_data(&mut client, cll_handle, vec![0x01], vec![]).await;
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        vec![0x10, 0x00, 0x03, 0x50, 0x01],
        "the first accept's own peer TX-ID should frame the first send"
    );

    // Spontaneous loss (device-side, unrelated to any teardown request from
    // this service) -- `Data[0..3]` (MSB first) = rx_id_passive, `Data[4]` =
    // reason (0).
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x00, 0x00, 0x03, 0x50, 0x00],
        j2534_0404::PROTOCOL_TP2_0_PS,
        0x0002_0000, // RX_FLAG_CONNECTION_LOST
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Re-listen: a NEW inbound accept, on the SAME rx_id_passive, with a
    // DIFFERENT peer TX-ID -- only possible if the persistent routing entry
    // survived the loss and this CLL's own phase re-entered `Listening`
    // (not a terminal `Lost`, which would leave nothing to route this
    // second accept to).
    assert!(
        server
            .backdoor
            .inject_tp20_passive_connection(MOCK_CHANNEL_ID, 0x1000_0351),
        "re-listening should succeed: the listener's own SET_CONFIG values are still valid and \
         the slot is free again after the spontaneous loss"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    send_data(&mut client, cll_handle, vec![0x02], vec![]).await;
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 1),
        vec![0x10, 0x00, 0x03, 0x51, 0x02],
        "the second accept's own NEW peer TX-ID should frame the second send, proving the \
         connection genuinely re-established"
    );

    server.shutdown().await;
}

/// Codex review finding (P2, PR #99, round 2): mirrors `stopcomm_stops_
/// this_clls_own_repeat_slots_before_native_teardown` (Fix D, active TP2.0)
/// and `reconcile_live_established_cll`'s own stop-on-spontaneous-loss
/// handling, but for a PASSIVE connection's own spontaneous loss: a repeat
/// message slot started while `Established` still carries the now-stale
/// peer TX-ID baked into its payload -- unlike an ordinary ComPrimitive,
/// `PDU_IOCTL_STOP_REPEAT_MESSAGE` is not tied to `comm_started`, so without
/// this fix it would keep autonomously retransmitting that stale TX-ID both
/// while the listener sits idle in `Listening` and after a new peer
/// establishes with a different TX-ID.
#[tokio::test]
#[serial]
async fn passive_connection_spontaneous_loss_stops_its_own_repeat_slot() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await
    );
    drop(events);

    assert!(
        server
            .backdoor
            .inject_tp20_passive_connection(MOCK_CHANNEL_ID, 0x1000_0350)
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Start a repeat message slot while the passive connection is
    // Established -- same setup `stopcomm_stops_this_clls_own_repeat_slots_
    // before_native_teardown` uses for the active-connection case.
    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let setup = DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup(
                5000,
                0,
                &[0x01, 0x02],
                &[0x00, 0x00, 0x00, 0x00],
                &[0x00, 0x00, 0x00, 0x00],
                &[],
            ),
        })),
    };
    let output = io_ctl_cll(&mut client, cll_handle, start_id, Some(setup), true)
        .await
        .expect(
            "PDU_IOCTL_START_REPEAT_MESSAGE should succeed on an Established passive TP2.0 CLL",
        );
    let msg_id = match output.and_then(|d| d.data) {
        Some(data_item::Data::Unum32Value(msg_id)) => msg_id,
        other => panic!(
            "PDU_IOCTL_START_REPEAT_MESSAGE should return a Unum32Value MsgId, got {other:?}"
        ),
    };
    assert!(
        server
            .backdoor
            .repeat_message_exists(MOCK_CHANNEL_ID, msg_id),
        "the repeat slot should be live immediately after START"
    );

    // Spontaneous loss (device-side, unrelated to any teardown request from
    // this service) -- `Data[0..3]` (MSB first) = rx_id_passive, `Data[4]` =
    // reason (0). Same injection shape `passive_connection_re_listens_
    // after_a_spontaneous_loss_and_re_establishes` uses.
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &[0x00, 0x00, 0x03, 0x50, 0x00],
        j2534_0404::PROTOCOL_TP2_0_PS,
        0x0002_0000, // RX_FLAG_CONNECTION_LOST
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert!(
        !server
            .backdoor
            .repeat_message_exists(MOCK_CHANNEL_ID, msg_id),
        "a passive connection's own spontaneous loss must stop this CLL's own repeat message \
         slot -- otherwise the adapter keeps autonomously retransmitting the now-stale peer \
         TX-ID as raw traffic while the listener re-enters Listening and after any new peer \
         establishes with a different TX-ID"
    );

    server.shutdown().await;
}

/// Codex review finding (P2, PR #99, round 3): `handle_stop_comm`'s
/// active-connection arm only stops this CLL's own repeat slots when
/// `tp20_teardown.is_some()`, which is deliberately `None` for a passive
/// connection (`!c.passive` on its own filter) -- but the passive-disarm
/// arm never called `stop_repeat_slots_for_cll` either, so `CoptStopcomm`
/// on an Established passive connection with a live repeat slot left the
/// adapter autonomously retransmitting the now-stale peer TX-ID straight
/// through the disarm. Distinct from `passive_connection_spontaneous_loss_
/// stops_its_own_repeat_slot` above, which covers the device-side
/// spontaneous-loss path (`deliver_tp20_connection_indication`'s own `Lost`
/// arm) -- this one covers the client-driven `CoptStopcomm` disarm path
/// (`handle_stop_comm`'s `tp20_passive_disarm` arm) instead.
#[tokio::test]
#[serial]
async fn stopcomm_stops_an_established_passive_connections_own_repeat_slot() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await
    );

    assert!(
        server
            .backdoor
            .inject_tp20_passive_connection(MOCK_CHANNEL_ID, 0x1000_0350)
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Start a repeat message slot while the passive connection is
    // Established -- same setup the sibling spontaneous-loss test uses.
    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let setup = DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup(
                5000,
                0,
                &[0x01, 0x02],
                &[0x00, 0x00, 0x00, 0x00],
                &[0x00, 0x00, 0x00, 0x00],
                &[],
            ),
        })),
    };
    let output = io_ctl_cll(&mut client, cll_handle, start_id, Some(setup), true)
        .await
        .expect(
            "PDU_IOCTL_START_REPEAT_MESSAGE should succeed on an Established passive TP2.0 CLL",
        );
    let msg_id = match output.and_then(|d| d.data) {
        Some(data_item::Data::Unum32Value(msg_id)) => msg_id,
        other => panic!(
            "PDU_IOCTL_START_REPEAT_MESSAGE should return a Unum32Value MsgId, got {other:?}"
        ),
    };
    assert!(
        server
            .backdoor
            .repeat_message_exists(MOCK_CHANNEL_ID, msg_id),
        "the repeat slot should be live immediately after START"
    );

    // CoptStopcomm: cop_data empty, so no final message -- dispatch goes
    // straight to the passive-disarm sequence.
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
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStopcomm should finish"
    );

    assert!(
        !server
            .backdoor
            .repeat_message_exists(MOCK_CHANNEL_ID, msg_id),
        "CoptStopcomm must stop an Established passive connection's own repeat message slot \
         before/alongside disarming it -- otherwise the adapter keeps autonomously \
         retransmitting the now-stale peer TX-ID as raw traffic after the disarm"
    );

    drop(events);
    server.shutdown().await;
}

/// Item 5 (ADR-190 section 4, the unconditional-quarantine convention):
/// `CoptStopcomm`'s disarm sequence races an inbound accept that has
/// already landed device-side (a real native slot is genuinely allocated)
/// but whose `CONNECTION_ESTABLISHED` indication the service has not yet
/// observed -- constructed deterministically via `__mock_set_tp20_no_
/// indication` (ADR-188's own toggle, reused here rather than depending on
/// the channel poll task's own timing, the same determinism precedent this
/// file's `disconnect_mid_wait_after_native_issue_tears_down_the_leaked_
/// slot` doc comment already establishes for an analogous race). With the
/// service genuinely unaware, `disarm_tp20_passive_listener`'s own
/// `was_established` read is `false`, so step 2's teardown call never runs
/// -- steps 1/3/4 still fire unconditionally, marking the persistent entry
/// `abandoned` and clearing the exclusivity token, proving the disarm
/// itself still completes cleanly (`PduCopstFinished`, not a hang) despite
/// the race.
///
/// **Bounded, not permanent, since ADR-190's Correction (Codex review
/// finding, P1, PR #99):** because `config_cleared` is `true` here too
/// (the mock's own `SET_CONFIG` calls always succeed), the disarm also
/// stamps `idle_release_at` -- this quarantine is released
/// `TP20_PASSIVE_IDLE_RELEASE_GRACE` (500ms) after the disarm, same as any
/// other never-established-at-disarm-time passive slot. This specific test
/// uses `__mock_set_tp20_no_indication`'s PERMANENT suppression (the
/// indication is never queued at all here, not merely delayed) purely as a
/// deterministic stand-in for the true race -- a real accept's indication
/// becoming queue-readable within the grace period, per ADR-190's own
/// clause-7.2.5 in-order-drain reasoning -- so this test only proves the
/// quarantine correctly holds THROUGH the grace window (the retry below
/// runs well inside it), not that it holds forever; a genuinely
/// non-conformant device that never delivers at all is the accepted,
/// documented residual ADR-190's Correction section names (the same
/// conformance-dependence class as ADR-188's own Fix AA residual), not
/// something this test can or should assert against. See
/// `disarm_of_a_never_established_passive_listener_bounded_releases_
/// after_the_grace_period` for the complementary proof that release
/// genuinely happens once the grace period elapses.
#[tokio::test]
#[serial]
async fn disarm_races_an_inbound_accept_and_leaves_the_entry_quarantined() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await
    );
    drop(events);

    // The service genuinely never learns about this accept (indication
    // suppressed for its entire lifetime) -- `was_established` stays
    // `false` for the disarm below, exactly as if the accept and the
    // disarm were racing and the disarm "won."
    server.backdoor.set_tp20_no_indication(true);
    assert!(
        server
            .backdoor
            .inject_tp20_passive_connection(MOCK_CHANNEL_ID, 0x1000_0350),
        "the native slot should still be genuinely allocated even with indications suppressed"
    );

    let mut events_stop = client
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
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) should succeed even racing the accept");
    assert!(
        wait_for_event(&mut events_stop, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStopcomm should finish cleanly (not hang) despite racing the accept"
    );
    drop(events_stop);
    server.backdoor.set_tp20_no_indication(false);
    // No settle sleep needed here (edge-case-hunter finding, PR #99: an
    // earlier version of this test slept 300ms against the 500ms
    // `TP20_PASSIVE_IDLE_RELEASE_GRACE` deadline, but the retry sequence's
    // own RPC round trips below already consume ~100ms+ of that margin,
    // leaving a real risk of intermittent failure under CI load --
    // ADR-149's own "poll, don't guess a margin" principle). None is
    // required: `quarantine_tp20_passive_slot_on_disarm` (including its
    // `idle_release_at` stamp) runs synchronously inside `handle_stop_
    // comm`'s own execution, strictly before the `PduCopstFinished` this
    // test already waited for above -- the quarantine state is therefore
    // already fully in place, deterministically, with no race to settle.

    // A fresh arm attempt reusing the SAME rx_id_passive is still rejected
    // -- issued immediately after the disarm's own COP finished, so it
    // runs well inside the grace window, before the bounded release could
    // ever fire.
    let cll_retry = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 2).await;
    let mut events_retry = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_retry)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_retry)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events_retry, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "the retry's own CoptStartcomm should finish (not hang)"
    );
    assert!(
        saw_rsc_locked,
        "rx_id_passive 0x0350 should still be quarantined immediately after the racing disarm, \
         well inside the 500ms bounded-release grace window, regardless of which side the poll \
         task happened to process first"
    );
    drop(events_retry);

    server.shutdown().await;
}

/// ADR-190's "Correction" paragraph under `### 4. Disarm / teardown` (Codex
/// review finding, P1, PR #99; design-advisor consult): a `CoptStopcomm`
/// disarm of a passive listener that is STILL `Listening` -- never accepted
/// any connection -- issues no native `TEARDOWN_CONNECTION` call at all
/// (`was_established` is `false`), so unlike
/// `disarm_races_an_inbound_accept_and_leaves_the_entry_quarantined` above
/// (a real native slot IS genuinely allocated there), nothing will ever
/// arrive to release the quarantine the ordinary way. The bounded release
/// this ADR adds (`Tp20ConnEntry::idle_release_at`,
/// `events_tp20_connection::release_idle_passive_slots`/`tp20_passive_
/// idle_release_due`) closes that leak: once the per-channel drain
/// watermark proves a full poll pass has completed past `TP20_PASSIVE_
/// IDLE_RELEASE_GRACE` (500 ms) since the disarm, the entry is removed and
/// a fresh CLL can re-arm the identical `identifier`/`rx_id_passive`.
///
/// The early-release RACE this mechanism guards against (a pre-disarm
/// accept's own indication not yet drained) is timing-infeasible to
/// construct deterministically end-to-end -- proven instead at the unit
/// level (`tp20_passive_idle_release_due`'s own tests, and `quarantine_
/// tp20_passive_slot_on_disarm`'s `idle_release_at`-stamping tests). This
/// test instead proves the OBSERVABLE end-to-end behavior the mechanism
/// exists to deliver: the ordinary (non-raced) "arm, never accept, disarm"
/// case eventually releases on its own rather than staying quarantined
/// forever, unlike `disarm_races_an_inbound_accept_and_leaves_the_entry_
/// quarantined` above.
#[tokio::test]
#[serial]
async fn disarm_of_a_never_established_passive_listener_bounded_releases_after_the_grace_period() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "the passive arm should complete immediately"
    );
    drop(events);

    // Disarm while still Listening -- no accept was ever injected, so
    // `was_established` stays `false` and step 2's teardown call never
    // runs (mirroring `disarm_races_an_inbound_accept_and_leaves_the_entry_
    // quarantined`'s own setup, minus the racing accept).
    let mut events_stop = client
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
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
        .expect("start_com_primitive(CoptStopcomm) should succeed");
    assert!(
        wait_for_event(&mut events_stop, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStopcomm should finish cleanly"
    );
    drop(events_stop);

    // Wait past TP20_PASSIVE_IDLE_RELEASE_GRACE (500ms) plus a settle
    // margin for the drain-watermark-gated poll-task sweep to observe and
    // act on the deadline -- this file's own established 300ms-per-round-
    // trip convention (`harness.rs`'s module doc, ADR-149), doubled here
    // since this window spans both the grace period itself and at least
    // one full tick of `release_idle_passive_slots`.
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;

    // A fresh CLL arming passive with the IDENTICAL identifier/rx_id_passive
    // succeeds -- possible only if the bounded release actually removed the
    // quarantine entry (unlike `disarm_races_an_inbound_accept_and_leaves_
    // the_entry_quarantined` above, where it never does).
    let cll_retry = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 2).await;
    let mut events_retry = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_retry)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_retry)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events_retry, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "the retry's own CoptStartcomm should finish (not hang)"
    );
    assert!(
        !saw_rsc_locked,
        "rx_id_passive 0x0350 should have been bounded-released after the grace period, letting \
         a fresh CLL re-arm the identical identifier/rx_id_passive -- a bare PduCopstFinished \
         alone does not distinguish genuine success from a rejected-but-completed attempt, so \
         this explicitly checks for the ABSENCE of PduErrEvtRscLocked too"
    );
    drop(events_retry);

    server.shutdown().await;
}

/// ADR-190 section 4: `DestroyComLogicalLink` disarms the passive listener
/// the same way `CoptStopcomm` does (`disarm_races_an_inbound_accept_and_
/// leaves_the_entry_quarantined` above covers the `CoptStopcomm` path) --
/// clearing `SharedChannel::tp20_passive` so a fresh CLL can arm passive
/// again on the identical `identifier`/`rx_id_passive`. A sibling keepalive
/// CLL keeps the physical channel's `ref_count` above zero across CLL A's
/// destroy, mirroring `disconnect_mid_wait_after_native_issue_tears_down_
/// the_leaked_slot`'s own precondition -- otherwise the passive slot's
/// release would be indistinguishable from the whole `SharedChannel` (and
/// its `tp20_passive` field) simply ceasing to exist.
///
/// Deliberately establishes CLL A's own passive connection (injects an
/// accept and waits for it) BEFORE destroying, rather than destroying a
/// merely-`Listening` slot -- an edge-case-hunter finding on an earlier
/// version of this diff caught that `rpc_link.rs`'s pre-existing
/// active-connection teardown filter had not been narrowed to exclude a
/// `passive` connection, so an `Established` passive connection matched
/// BOTH the active teardown block and the new passive-disarm block,
/// issuing `IOCTL_TEARDOWN_CONNECTION` twice (the second call always
/// failing, since the mock already removed the slot on the first) and
/// quarantining `rx_id_passive` via the WRONG helper
/// (`quarantine_tp20_connection_for_orphaned_write_back`, which stamps the
/// new entry `passive: false`). A `Listening`-only version of this test
/// cannot reach either teardown block at all (`was_established` gates
/// them both), so it passed regardless of that bug -- exactly the gap
/// that finding flagged. Fixed in `rpc_link.rs` by adding `!c.passive` to
/// both active-block filters, matching `handle_stop_comm`'s own guard.
#[tokio::test]
#[serial]
async fn destroy_com_logical_link_disarms_the_passive_listener() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll_keepalive = create_and_connect_tp20_cll(&mut client, 0x0099, 0).await;

    let cll_a = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 1).await;
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events_a, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's own passive arm should complete immediately"
    );

    drop(events_a);

    // Establish it for real before destroying -- see this test's own doc
    // comment for why a merely-armed (`Listening`) slot would not exercise
    // the bug this test now guards against. No client-visible establish
    // event exists (ADR-190 Consequences), so this gives the poll task a
    // few ticks to drain the injected indication (POLL_INTERVAL_MS = 10ms),
    // the same technique `passive_listener_arms_immediately_then_
    // establishes_and_exchanges_data` above uses.
    assert!(
        server
            .backdoor
            .inject_tp20_passive_connection(MOCK_CHANNEL_ID, 0x1000_0350),
        "the inbound accept should land while CLL A's own listener is armed"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("destroy_com_logical_link should succeed");
    assert_eq!(
        server.backdoor.disconnect_count(),
        0,
        "the physical channel must survive CLL A's own destroy (the keepalive CLL still holds \
         it open) -- this test needs the SAME SharedChannel to still be reachable by CLL B \
         below, so a real leak here would indicate this test's own setup is unintentionally \
         tearing down the physical channel instead of exercising the disarm-on-destroy path"
    );

    // Give the destroy path's own best-effort IOCTL_TEARDOWN_CONNECTION call
    // and its resulting (unconditionally-quarantined) CONNECTION_LOST
    // indication one full poll cycle to drain naturally -- unlike
    // `disarm_races_an_inbound_accept_and_leaves_the_entry_quarantined`
    // above, nothing here suppresses indications, so the quarantine is
    // expected to release on its own, not stay permanent.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // A fresh CLL arming passive with the IDENTICAL identifier/rx_id_passive
    // succeeds -- possible only if CLL A's own destroy actually cleared
    // `SharedChannel::tp20_passive` AND its own quarantine entry
    // subsequently released (both `!c.passive`-filter-dependent: with the
    // pre-fix double-teardown bug, the entry would end up misattributed to
    // the ACTIVE quarantine helper as `passive: false`, and this same class
    // of RX-ID reuse would either behave incorrectly or race unpredictably).
    let cll_b = create_and_connect_tp20_cll_passive(&mut client, 0x0250, 0x0350, 2).await;
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events_b, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "CLL B's own CoptStartcomm should finish (not hang)"
    );
    assert!(
        !saw_rsc_locked,
        "CLL B should be able to re-arm the passive slot after CLL A's own destroy released it \
         and its quarantine entry drained -- a bare PduCopstFinished alone does not distinguish \
         genuine success from a rejected-but-completed attempt, so this explicitly checks for \
         the ABSENCE of PduErrEvtRscLocked too"
    );
    drop(events_b);

    server.shutdown().await;
}

/// The `_CHx` analog of `destroy_com_logical_link_disarms_the_passive_
/// listener` above: proves `DestroyComLogicalLink`'s own
/// passive-listener-disarm gate (`rpc_link.rs`, ADR-210 Decision item 11)
/// actually clears `SharedChannel::tp20_passive` for a `_CHx`-connected
/// TP2.0 CLL, instead of leaving the passive slot permanently armed. Before
/// the fix, this gate was keyed on the narrow `is_tp2_0_protocol_id` (an
/// exact `_PS`-only match), so a `_CHx`-connected passive CLL's own
/// `hw_protocol_id` (`PROTOCOL_TP2_0_CH1`, not `PROTOCOL_TP2_0_PS`) would
/// never satisfy it -- `DestroyComLogicalLink` would tear down the CLL's own
/// service-side bookkeeping but never disarm the native passive listener,
/// permanently occupying the channel's one passive slot with no way for a
/// fresh CLL to re-arm on the identical `identifier`/`rx_id_passive`.
///
/// Same structure as the `_PS` precedent: a `_CHx`-connected keepalive CLL
/// keeps the physical channel's `ref_count` above zero across CLL A's
/// destroy (otherwise the passive slot's release would be indistinguishable
/// from the whole `SharedChannel` ceasing to exist); CLL A's own passive
/// connection is established for real (not merely armed) before destroying,
/// mirroring the `_PS` precedent's own reasoning for why a merely-`Listening`
/// slot would not exercise the same code path; and the proof is a fresh CLL
/// B re-arming on the identical `identifier`/`rx_id_passive` and completing
/// its own `CoptStartcomm` without `PduErrEvtRscLocked`.
#[tokio::test]
#[serial]
async fn chx_destroy_com_logical_link_disarms_the_passive_listener() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll_keepalive = create_and_connect_tp20_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_TP2_0_CH1,
        &[],
        0x0210,
        0,
    )
    .await;

    let cll_a = create_and_connect_tp20_cll_passive_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_TP2_0_CH1,
        &[],
        0x0270,
        0x0370,
        1,
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_TP2_0_CH1,
        "this test must actually exercise a _CHx-connected physical channel, not silently fall \
         back to _PS"
    );
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events_a, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's own passive arm should complete immediately"
    );

    drop(events_a);

    // Establish it for real before destroying -- see
    // `destroy_com_logical_link_disarms_the_passive_listener`'s own doc
    // comment for why a merely-armed (`Listening`) slot would not exercise
    // the bug this test guards against.
    assert!(
        server
            .backdoor
            .inject_tp20_passive_connection(MOCK_CHANNEL_ID, 0x1000_0370),
        "the inbound accept should land while CLL A's own listener is armed"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("destroy_com_logical_link should succeed");
    assert_eq!(
        server.backdoor.disconnect_count(),
        0,
        "the physical channel must survive CLL A's own destroy -- the keepalive CLL still holds \
         it open -- this test needs the SAME SharedChannel to still be reachable by CLL B below, \
         so a real leak here would indicate this test's own setup is unintentionally tearing \
         down the physical channel instead of exercising the disarm-on-destroy path"
    );

    // Give the destroy path's own best-effort IOCTL_TEARDOWN_CONNECTION call
    // and its resulting (unconditionally-quarantined) CONNECTION_LOST
    // indication one full poll cycle to drain naturally.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // A fresh CLL arming passive with the IDENTICAL identifier/rx_id_passive
    // succeeds only if CLL A's own destroy actually cleared
    // `SharedChannel::tp20_passive` AND its own quarantine entry
    // subsequently released.
    let cll_b = create_and_connect_tp20_cll_passive_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_TP2_0_CH1,
        &[],
        0x0270,
        0x0370,
        2,
    )
    .await;
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events_b, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "CLL B's own CoptStartcomm should finish (not hang)"
    );
    assert!(
        !saw_rsc_locked,
        "CLL B should be able to re-arm the passive slot after CLL A's own destroy released it \
         and its quarantine entry drained -- a bare PduCopstFinished alone does not distinguish \
         genuine success from a rejected-but-completed attempt, so this explicitly checks for \
         the ABSENCE of PduErrEvtRscLocked too. Before the ADR-210 rpc_link.rs fix, this \
         assertion would fail: CLL A's own passive slot would still be armed (never disarmed, \
         since `is_tp2_0_protocol_id` never matched its _CHx hw_protocol_id), so CLL B's own \
         re-arm attempt on the identical identifier/rx_id_passive would be rejected"
    );
    drop(events_b);

    server.shutdown().await;
}

/// Mock-fidelity coverage (ADR-190 Consequences): an inbound accept
/// injected against a channel whose passive listener was never armed
/// (`CONFIG_TP2_0_IDENTIFER`/`_RXIDPASSIVE` still `0`, the native default)
/// silently no-ops -- no `rx_queue` push, mirroring clause 19.3.3.1's own
/// network-side rejection never reaching the application as an indication
/// -- and increments `__mock_get_passive_connection_rejected_count` for
/// test observability, since nothing else about a silent no-op would
/// otherwise be visible from outside the mock.
#[tokio::test]
#[serial]
async fn injecting_a_passive_connection_before_arming_is_silently_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    // Connect a TP2.0 CLL without ever staging/arming the passive listener
    // -- CONFIG_TP2_0_IDENTIFER/_RXIDPASSIVE stay at their native default
    // (0, disabled).
    let _cll_handle = create_tp20_cll(&mut client, TP2_0_RESOURCE_ID, TP2_0_PINS, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(_cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(
        server
            .backdoor
            .passive_connection_rejected_count(MOCK_CHANNEL_ID),
        0
    );
    assert!(
        !server
            .backdoor
            .inject_tp20_passive_connection(MOCK_CHANNEL_ID, 0x1000_0350),
        "the injection must silently no-op -- the listener was never armed"
    );
    assert_eq!(
        server
            .backdoor
            .passive_connection_rejected_count(MOCK_CHANNEL_ID),
        1,
        "the rejection counter should observe the no-op"
    );

    server.shutdown().await;
}

// ── SAE J2534-2 clause 19.3.2.2/19.3.2.3 TP2.0 broadcast frames and
// periodic re-trigger (ADR-192/Phase 7 Stage 7c) ───────────────────────────

/// D-PDU `CP_TP20BroadcastAddress` ComParam ID
/// (`service_params::PARAM_TP20_BROADCAST_ADDRESS`, 0x80D0, ADR-192/Phase 7
/// Stage 7c): the per-send-scoped broadcast address, staged via
/// `SetComParam` and resolved against Working for a `temp_param_update`
/// `CoptSendrecv` (ADR-067's existing mechanism).
const CP_TP20_BROADCAST_ADDRESS: u32 = 0x80D0;

/// D-PDU `CP_TP20BroadcastInterval` ComParam ID
/// (`service_params::PARAM_TP20_BROADCAST_INTERVAL`, 0x80D1, ADR-192/Phase 7
/// Stage 7c): the physical-layer interval between a broadcast burst's five
/// frames, routed to native `CONFIG_TP2_0_T_BR_INT` (`j2534_0404::
/// CONFIG_TP2_0_T_BR_INT`) through the generic ComParam-to-`SET_CONFIG`
/// pipeline.
const CP_TP20_BROADCAST_INTERVAL: u32 = 0x80D1;

/// Issues a `CoptSendrecv` with full control over `num_send_cycles`/
/// `num_receive_cycles`/`temp_param_update`/`expected_response_array` --
/// `harness.rs`'s own `send_data`/`arm_receive_only_monitor` helpers each
/// hardcode a fixed shape, which the broadcast tests below need to vary
/// (temp-scoped ComParam staging, and the cyclic `-1` periodic re-trigger
/// case).
#[allow(clippy::too_many_arguments)]
async fn start_send_recv(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: ComLogicalLinkHandle,
    cop_data: Vec<u8>,
    time: u32,
    num_send_cycles: i32,
    num_receive_cycles: i32,
    temp_param_update: u32,
    expected_response_array: Vec<ExpectedResponseData>,
) -> Result<ComPrimitiveHandle, tonic::Status> {
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time,
                num_send_cycles,
                num_receive_cycles,
                temp_param_update,
                expected_response_array,
                tx_flag: None,
            }),
        })
        .await
        .map(|resp| {
            resp.into_inner()
                .cop_handle
                .expect("cop_handle should be present")
        })
}

/// Decision item 1: a broadcast `CoptSendrecv` (`CP_TP20BroadcastAddress`
/// staged via `temp_param_update`, ADR-067's existing per-send-scoping
/// mechanism) composes `[address] ++ payload` and sends it, and the mock
/// simulates the device's own 5x burst (clause 19.3.2.2) -- asserted here
/// with NO `CoptStartcomm` ever issued on this CLL, proving Decision item
/// 1's "no Established-connection precondition for a broadcast" directly,
/// not just via the unit tests in `tx_header.rs`.
#[tokio::test]
#[serial]
async fn broadcast_send_via_temp_comparam_writes_five_alternating_frames() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    set_com_param_unum32(&mut client, cll_handle, CP_TP20_BROADCAST_ADDRESS, 0xF3).await;

    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x01, 0x02],
        0,
        1,
        0,
        1, // temp_param_update
        Vec::new(),
    )
    .await
    .expect("broadcast CoptSendrecv should be accepted");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "a single-shot broadcast burst should finish normally via the ordinary send path"
    );

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        5,
        "a single PassThruWriteMsgs broadcast call must simulate a 5-frame burst"
    );
    for i in 0..5usize {
        let data = server.backdoor.written_data(MOCK_CHANNEL_ID, i);
        assert_eq!(
            data[0], 0xF3,
            "frame {i}'s leading byte must be the broadcast address"
        );
        let expected = if i % 2 == 0 { 0xAA } else { 0x55 };
        assert_eq!(
            &data[data.len() - 2..],
            &[expected, expected],
            "frame {i}'s last two bytes must alternate 0xAA/0x55"
        );
    }

    drop(events);
    server.shutdown().await;
}

/// Codex review fix (P1, PR #101): a single-shot broadcast `CoptSendrecv`
/// issued on a CLL that ALSO has an established TP2.0 connection (unlike the
/// test above, which never issues `CoptStartcomm` at all). ADR-192 Decision
/// item 1: broadcast is per-send, connection-independent -- a CLL's
/// connection stays established the whole time a broadcast is staged and
/// sent. Before this fix, `resolve_send_recv_tx` captured
/// `tp20_established_tx_id` from the CLL's live connection state
/// independent of whether this particular send was a broadcast, and
/// `events.rs`'s dispatch-time TOCTOU-closing logic (ADR-188 fix, PR #97)
/// unconditionally overwrote `data[0..4]` with that established TX-ID
/// whenever the connection had not drifted -- corrupting the broadcast
/// address byte plus 3 payload bytes on every send. Asserts the wire frames
/// still carry the broadcast address, not the established TX-ID bytes.
#[tokio::test]
#[serial]
async fn broadcast_send_on_a_cll_with_an_established_connection_is_not_corrupted() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // Establish the connection first (unlike the "no CoptStartcomm at all"
    // test above) -- the mock assigns TX-ID 0x1000_0321 for this rx_id
    // proposal (0x0321), the exact bytes the pre-fix overwrite would have
    // clobbered the broadcast frame with.
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish (PduCopstFinished) once the connection establishes"
    );

    set_com_param_unum32(&mut client, cll_handle, CP_TP20_BROADCAST_ADDRESS, 0xF3).await;

    start_send_recv(
        &mut client,
        cll_handle,
        vec![0x01, 0x02],
        0,
        1,
        0,
        1, // temp_param_update
        Vec::new(),
    )
    .await
    .expect("broadcast CoptSendrecv should be accepted on a CLL with an established connection");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "a single-shot broadcast burst should finish normally via the ordinary send path"
    );

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        5,
        "a single PassThruWriteMsgs broadcast call must simulate a 5-frame burst"
    );
    for i in 0..5usize {
        let data = server.backdoor.written_data(MOCK_CHANNEL_ID, i);
        assert_eq!(
            data[0], 0xF3,
            "frame {i}'s leading byte must be the broadcast address, not overwritten with the \
             established TX-ID (this CLL's own live connection: 0x1000_0321) -- the dispatch-time \
             TP2.0-connection-awareness overwrite must be skipped entirely for a broadcast item"
        );
        assert_ne!(
            &data[..2],
            &[0x10, 0x00],
            "frame {i} must never carry the established TX-ID's leading bytes"
        );
    }

    drop(events);
    server.shutdown().await;
}

/// Decision item 1: an out-of-range (nonzero, outside `0xF0-0xFF`)
/// `CP_TP20BroadcastAddress` is rejected synchronously as `INVALID_ARGUMENT`
/// before any native call -- never silently treated as "no broadcast".
#[tokio::test]
#[serial]
async fn broadcast_send_with_out_of_range_address_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TP20_BROADCAST_ADDRESS, 0x05).await;

    let err = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x01, 0x02],
        0,
        1,
        0,
        1,
        Vec::new(),
    )
    .await
    .expect_err("an out-of-range CP_TP20BroadcastAddress must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "a rejected broadcast must never reach the wire"
    );

    server.shutdown().await;
}

/// Codex review fix (P2, PR #101, round 9, ADR-192/Phase 7 Stage 7c): SAE
/// J2534-2 clause 14 Repeat Messaging and the TP2.0 broadcast mechanism
/// (clause 19.3.2.2/19.3.2.3) are not a supported combination --
/// `tx_header::build_tx_message`'s TP2.0 arm would otherwise compose
/// `[address] ++ payload` broadcast framing whenever `CP_TP20BroadcastAddress`
/// is staged in Active, regardless of caller intent, while
/// `ioctl_start_repeat_message`'s own TxFlags composition has no broadcast
/// awareness at all. `CP_TP20BroadcastAddress` can be staged in Active via a
/// PLAIN `SetComParam` + `CoptUpdateparam`, entirely independent of ever
/// issuing a broadcast COP -- so `PDU_IOCTL_START_REPEAT_MESSAGE` must reject
/// it explicitly rather than silently compose a malformed frame.
#[tokio::test]
#[serial]
async fn start_repeat_message_is_rejected_while_a_broadcast_address_is_staged() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TP20_BROADCAST_ADDRESS, 0xF5).await;
    promote_via_update_param(&mut client, cll_handle).await;

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let setup = DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup(
                5000,
                0,
                &[0x01, 0x02],
                &[0x00, 0x00, 0x00, 0x00],
                &[0x00, 0x00, 0x00, 0x00],
                &[],
            ),
        })),
    };
    let err = io_ctl_cll(&mut client, cll_handle, start_id, Some(setup), true)
        .await
        .expect_err(
            "PDU_IOCTL_START_REPEAT_MESSAGE must be rejected while CP_TP20BroadcastAddress is \
             staged in Active",
        );
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    server.shutdown().await;
}

/// Codex review round 19 (P2, PR #101): the sibling to the test above, but
/// staging an OUT-OF-RANGE nonzero `CP_TP20BroadcastAddress` (`0x01`, never
/// `0xF0-0xFF`) instead of a valid one. Before this fix, the rejection guard
/// consulted the normalized `ComParamSet::tp20_broadcast_address()`
/// accessor, which folds an out-of-range nonzero value to `None` (its own
/// doc comment says the RPC layer is responsible for rejecting such a value
/// BEFORE this accessor is ever consulted -- true for the COP send paths via
/// `validate_tp20_broadcast_address_range`, but Repeat Messaging never
/// routed through that check at all) -- so this exact scenario slipped past
/// the guard silently and reached `build_tx_message`'s broadcast-framing
/// composition with ordinary connection-bound TxFlags. The guard now checks
/// the raw staged value directly, rejecting any nonzero value regardless of
/// range, since Repeat Messaging has no legitimate broadcast use for one.
#[tokio::test]
#[serial]
async fn start_repeat_message_is_rejected_while_an_out_of_range_broadcast_address_is_staged() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TP20_BROADCAST_ADDRESS, 0x01).await;
    promote_via_update_param(&mut client, cll_handle).await;

    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let setup = DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup(
                5000,
                0,
                &[0x01, 0x02],
                &[0x00, 0x00, 0x00, 0x00],
                &[0x00, 0x00, 0x00, 0x00],
                &[],
            ),
        })),
    };
    let err = io_ctl_cll(&mut client, cll_handle, start_id, Some(setup), true)
        .await
        .expect_err(
            "PDU_IOCTL_START_REPEAT_MESSAGE must be rejected while an out-of-range nonzero \
             CP_TP20BroadcastAddress is staged in Active, not just an in-range one",
        );
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    server.shutdown().await;
}

/// Fix 4 (Codex review, P2, PR #101, ADR-192/Phase 7 Stage 7c): the same raw
/// out-of-range `CP_TP20BroadcastAddress` rejection the previous test
/// exercises for `CoptSendrecv` also applies to `CoptStartcomm`'s own
/// optional message -- before this fix, that path reached
/// `resolve_send_recv_tx` directly, so `ComParamSet::tp20_broadcast_address`'s
/// own silent out-of-range-to-`None` fold let a garbage nonzero value slip
/// through and send as an ordinary connection-bound frame instead of being
/// rejected.
#[tokio::test]
#[serial]
async fn startcomm_optional_message_with_out_of_range_broadcast_address_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TP20_BROADCAST_ADDRESS, 0x05).await;

    let err = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x01, 0x02],
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
            "a CoptStartcomm optional message on a TP2.0 CLL with an out-of-range nonzero \
             CP_TP20BroadcastAddress staged must be rejected, not silently sent as an ordinary \
             frame",
        );
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "a rejected CoptStartcomm optional message must never reach the wire"
    );

    server.shutdown().await;
}

/// ADR-192 Consequences: a broadcast `CoptSendrecv` requesting a finite
/// repeat count other than the single-shot burst (`num_send_cycles == 1`) or
/// the infinite periodic re-trigger (`-1`) is rejected -- no native
/// primitive exists for a finite repeat count greater than the fixed
/// five-frame burst.
#[tokio::test]
#[serial]
async fn broadcast_send_with_unsupported_finite_num_send_cycles_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TP20_BROADCAST_ADDRESS, 0xF3).await;

    let err = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x01, 0x02],
        0,
        3,
        0,
        1,
        Vec::new(),
    )
    .await
    .expect_err(
        "a broadcast CoptSendrecv with num_send_cycles == 3 (unsupported finite repeat) \
             must be rejected",
    );
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "a rejected broadcast must never reach the wire"
    );

    server.shutdown().await;
}

/// ADR-192 Consequences: a broadcast `CoptSendrecv` asking for a response
/// (`num_receive_cycles != 0`) is rejected outright -- no response is
/// attributable to a broadcast send under this service's per-connection
/// routing.
#[tokio::test]
#[serial]
async fn broadcast_send_with_nonzero_num_receive_cycles_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_TP20_BROADCAST_ADDRESS, 0xF3).await;

    let err = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x01, 0x02],
        0,
        1,
        1,
        1,
        Vec::new(),
    )
    .await
    .expect_err("a broadcast CoptSendrecv with num_receive_cycles != 0 must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// Starts a broadcast periodic re-trigger (`num_send_cycles == -1`,
/// `CP_TP20BroadcastAddress` staged, `time` = the periodic rate) and returns
/// the started `cop_handle`. Shared by the periodic-lifecycle tests below.
async fn start_broadcast_periodic(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: ComLogicalLinkHandle,
    address: u32,
) -> ComPrimitiveHandle {
    set_com_param_unum32(client, cll_handle, CP_TP20_BROADCAST_ADDRESS, address).await;
    start_send_recv(
        client,
        cll_handle,
        vec![0x01, 0x02],
        50, // periodic rate, ms
        -1,
        0,
        1, // temp_param_update
        Vec::new(),
    )
    .await
    .expect("broadcast periodic CoptSendrecv should be accepted")
}

/// Decision item 2: a cyclic broadcast `CoptSendrecv` (`num_send_cycles ==
/// -1`) maps to a real native `PassThruStartPeriodicMsg` call -- issued
/// directly, bypassing the ordinary tx_queue/poll-task dispatch pipeline
/// (never a `PassThruWriteMsgs` loop). Asserted via the mock's own
/// device-side state (`start_periodic_count`/`periodic_msg_count`), not just
/// this service's internal tracking, plus the same immediate 5-frame burst
/// `PassThruWriteMsgs`'s own broadcast case produces.
#[tokio::test]
#[serial]
async fn broadcast_periodic_starts_native_periodic_message_with_immediate_burst() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let cop_handle = start_broadcast_periodic(&mut client, cll_handle, 0xFF).await;

    wait_for_cop_executing(&mut client, cop_handle).await;

    assert_eq!(
        server.backdoor.start_periodic_count(),
        1,
        "a cyclic broadcast CoptSendrecv must issue exactly one native \
         PassThruStartPeriodicMsg call"
    );
    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        1,
        "the native adapter should have exactly one live periodic message"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        5,
        "the periodic start's own immediate burst sends 5 frames, same as \
         PassThruWriteMsgs's broadcast case"
    );

    server.shutdown().await;
}

/// Fix 5 (Codex review, P2, PR #101, ADR-192/Phase 7 Stage 7c): a
/// `temp_param_update=1` broadcast periodic start temp-applies the Working
/// ComParam snapshot to hardware before the native `PassThruStartPeriodicMsg`
/// call, then reverts to the live Active set immediately after -- the same
/// "borrow Working for one TX then revert" bracket `handle_send_recv` runs
/// around an ordinary transmit (ISO 22900-2 §9.4.3), applied here around the
/// ONE native periodic-message start action instead.
///
/// Unlike an ordinary `CoptSendrecv`/`CoptStartcomm` (which dispatch through
/// a background poll task, giving tests a real asynchronous window between
/// the apply and the revert to observe the intermediate hardware value —
/// see `pin_selection.rs`'s `iso9141_ps_temp_param_update_sendrecv_reverts_
/// tidle_to_active`), a broadcast periodic start is fully synchronous within
/// `rpc_start_com_primitive` itself (bypasses the tx_queue/poll-task
/// pipeline entirely, ADR-192 Decision item 2) -- apply, native start, and
/// revert all complete before the RPC ever returns to this test. There is no
/// deterministic point at which a black-box gRPC/backdoor poll could observe
/// the momentarily-applied Working value without racing wall-clock
/// scheduling (exactly what `harness.rs`'s module doc / ADR-149 rules out).
/// This test instead asserts the three deterministic, non-racy parts of the
/// same contract: (1) the apply-then-revert bracket actually ran exactly
/// once around this one call (`set_config_param_log`'s
/// `CONFIG_TP2_0_T_BR_INT` occurrence count increases by exactly 2 -- one
/// push, one revert; it would stay unchanged if Fix 5 were absent, since
/// nothing would touch hardware for this call at all), (2) hardware ends up
/// back on the pre-bracket CAPTURED value afterward (round 18, ADR-192
/// Decision item 3 amendment: `CP_TP20BroadcastInterval` is channel-wide,
/// so its revert restores `capture_channel_wide_hardware_locked`'s captured
/// pre-bracket hardware read, not this CLL's own per-CLL Active snapshot --
/// see `comparam_support::strip_captured_channel_wide_keys` for the
/// mechanism; this single-CLL scenario cannot itself distinguish "captured"
/// from "Active" since nothing else ever wrote hardware between connect and
/// this bracket, so the two numerically coincide at 20 here -- see
/// `broadcast_periodic_temp_param_update_reverts_to_sibling_captured_value_not_stale_active`
/// below for a scenario where they differ), not stuck on Working's, and
/// (3) -- Codex review round 8 Finding 1 -- the apply batch and revert
/// batch are themselves ADJACENT in the log, with no other
/// channel-affecting `SET_CONFIG` op interleaved between them.
///
/// Part (3)'s adjacency check cannot force the actual concurrent interleave
/// the round-8 fix closes (a sibling CLL's own temp-bound broadcast start,
/// or a queued `CoptUpdateparam`, landing between the apply and the native
/// start, or between the native start and the revert): this test issues one
/// synchronous client request, so nothing else genuinely runs concurrently
/// with it. `ADR-110`'s own amendment ("Lock-grant/apply serialization",
/// Finding 2) accepted the identical limitation for its own analogous
/// `handle_update_param` fix, for the same reason (`events.rs`'s own doc
/// comment on that critical section) -- a true repro needs a production
/// yield hook this codebase does not have. This test instead pins the
/// structural invariant the fix guarantees: `working`/`active` differ ONLY
/// in `CP_TP20BroadcastInterval`'s value here, so the apply batch and the
/// revert batch push the exact same SET_CONFIG *ids* in the exact same
/// order -- if the two batches are genuinely contiguous (nothing else
/// logged in between, and nothing split either batch itself), the log delta
/// for this one call must be two back-to-back copies of the same id
/// sequence. Reasoning about the OLD code (three separate
/// `self.api.lock().await` acquisitions, real gaps between each): this
/// assertion would have been just as likely to pass there for THIS
/// single-request test specifically (no concurrent client to exploit the
/// gaps), so it is not a literal before/after repro either -- but it does
/// pin the mechanism's steady-state log shape going forward, and the doc
/// comment above makes the actual concurrency-closing claim explicit rather
/// than silently relying on this test to prove it.
#[tokio::test]
#[serial]
async fn broadcast_periodic_temp_param_update_applies_and_reverts_broadcast_interval() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    // Sanity: Table 77's own default (20ms, ADR-192 Decision item 3) should
    // already be Active and pushed to hardware at connect.
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        20,
        "sanity check: CP_TP20BroadcastInterval's 20ms default should already be applied at \
         connect"
    );

    // Stage a DIFFERENT Working value than Active's -- the temp bracket
    // below applies this one temporarily, then must revert hardware back to
    // the captured pre-bracket hardware value (20 here -- see this test's
    // own doc comment for why this single-CLL scenario cannot distinguish
    // "captured" from "this CLL's own Active").
    set_com_param_unum32(&mut client, cll_handle, CP_TP20_BROADCAST_INTERVAL, 250).await;

    let full_log_before = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    let log_before = full_log_before
        .iter()
        .filter(|&&id| id == j2534_0404::CONFIG_TP2_0_T_BR_INT)
        .count();

    let cop_handle = start_broadcast_periodic(&mut client, cll_handle, 0xFF).await;
    wait_for_cop_executing(&mut client, cop_handle).await;

    assert_eq!(
        server.backdoor.start_periodic_count(),
        1,
        "the native periodic start must still succeed with a temp-bound broadcast interval"
    );

    let full_log_after = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    let log_after = full_log_after
        .iter()
        .filter(|&&id| id == j2534_0404::CONFIG_TP2_0_T_BR_INT)
        .count();
    assert_eq!(
        log_after,
        log_before + 2,
        "the temp-apply-then-revert bracket should push CONFIG_TP2_0_T_BR_INT exactly twice \
         around this one broadcast periodic start (once for the Working apply, once for the \
         captured-restore revert)"
    );

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        20,
        "hardware must be reverted to the captured pre-bracket CP_TP20BroadcastInterval (20ms) \
         immediately after the periodic start, not left on the temp-applied Working value \
         (250ms) -- this scenario's captured value and this CLL's own Active happen to \
         numerically coincide at 20, so this assertion alone cannot discriminate the two \
         mechanisms (see this test's own doc comment)"
    );

    // Codex review round 8 Finding 1: everything this ONE call appended to
    // the log (the apply batch immediately followed by the revert batch,
    // nothing else) must split into two equal-length, IDENTICAL id
    // sequences -- Working and Active differ only in
    // CP_TP20BroadcastInterval's value here, never in which keys are
    // staged, so the apply batch's own id list and the revert batch's own
    // id list must be the same ids in the same order. Any other
    // channel-affecting SET_CONFIG op interleaving between the two batches
    // (or splitting either one) would break this equality.
    let delta = &full_log_after[full_log_before.len()..];
    assert!(
        !delta.is_empty() && delta.len() % 2 == 0,
        "expected exactly two same-length SET_CONFIG batches (apply, revert) in the delta, got \
         {delta:?}"
    );
    let batch_len = delta.len() / 2;
    assert_eq!(
        delta[..batch_len],
        delta[batch_len..],
        "the apply batch and the revert batch must be adjacent, identical-id sequences in the \
         log -- no other channel-affecting SET_CONFIG op may interleave between them, and \
         neither batch may itself be split by one"
    );

    server.shutdown().await;
}

/// Strengthens `broadcast_periodic_temp_param_update_applies_and_reverts_
/// broadcast_interval` above (edge-case-hunter adversarial review, PR #101,
/// minor Finding 4): that test's captured-restore value and CLL A's own
/// per-CLL Active happen to numerically coincide (both 20), so it cannot
/// itself discriminate a captured-restore revert from a (pre-round-18,
/// buggy) Active-derived one. This test builds the scenario where they
/// differ, mirroring `events_revert_hardware_to_live_active_locked_tests.rs`'s
/// `sibling_cll_channel_wide_clobber_is_avoided_by_the_captured_restore`
/// unit test, but end-to-end through the real gRPC/native-mock path: CLL B
/// (a sibling sharing CLL A's physical channel) promotes its own
/// `CP_TP20BroadcastInterval` to a distinct, currently-live hardware value
/// via an ordinary `CoptUpdateparam` -- standing in for "the channel's real
/// value has moved on since CLL A's own stale Active was last written" --
/// and CLL A then runs its own `temp_param_update=1` broadcast periodic
/// start/revert bracket. If the revert restored CLL A's own (stale) Active
/// instead of the captured pre-bracket hardware value, hardware would end
/// up on CLL A's stale Active's value; this test proves it instead ends up
/// back on CLL B's live, promoted value.
#[tokio::test]
#[serial]
async fn broadcast_periodic_temp_param_update_reverts_to_sibling_captured_value_not_stale_active() {
    const CLL_A_STALE_ACTIVE_BROADCAST_INTERVAL: u32 = 20; // Table 77's default, never changed on A.
    const CLL_B_LIVE_BROADCAST_INTERVAL: u32 = 99; // B's own promoted, currently-live value.
    const CLL_A_TEMP_BROADCAST_INTERVAL: u32 = 250; // A's own temp-bound Working value.

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let cll_b = create_and_connect_tp20_cll(&mut client, 0x0322, 2).await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        CLL_A_STALE_ACTIVE_BROADCAST_INTERVAL,
        "sanity check: Table 77's own 20ms default should still be live on the shared physical \
         channel before CLL B promotes its own distinct value"
    );

    // CLL B promotes its own, distinct CP_TP20BroadcastInterval to Active --
    // standing in for a prior, already-completed CoptUpdateparam that moved
    // the channel's real, currently-live value away from CLL A's own
    // (never-updated) Active copy.
    set_com_param_unum32(
        &mut client,
        cll_b,
        CP_TP20_BROADCAST_INTERVAL,
        CLL_B_LIVE_BROADCAST_INTERVAL,
    )
    .await;
    promote_via_update_param(&mut client, cll_b).await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        CLL_B_LIVE_BROADCAST_INTERVAL,
        "sanity check: CLL B's own CoptUpdateparam should have pushed its distinct value to the \
         shared physical channel"
    );

    // CLL A's own bracket: stage a temp-bound Working value, start a
    // broadcast periodic (temp_param_update=1), and let it revert.
    set_com_param_unum32(
        &mut client,
        cll_a,
        CP_TP20_BROADCAST_INTERVAL,
        CLL_A_TEMP_BROADCAST_INTERVAL,
    )
    .await;
    let cop_handle = start_broadcast_periodic(&mut client, cll_a, 0xFF).await;
    wait_for_cop_executing(&mut client, cop_handle).await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_TP2_0_T_BR_INT),
        CLL_B_LIVE_BROADCAST_INTERVAL,
        "CLL A's own revert must restore the CAPTURED pre-bracket hardware value (CLL B's live \
         99), never CLL A's own stale per-CLL Active copy (20) -- this is the sibling-CLL \
         clobber bug ADR-192 Decision item 3's round-18 amendment closes, proven here through \
         the real gRPC/native-mock path rather than only the direct unit test"
    );

    server.shutdown().await;
}

/// Decision item 2/Consequences: `CoptCancel` of the owning COP stops the
/// live native periodic message (`PassThruStopPeriodicMsg`) -- asserted via
/// the mock's own device-side state, not just this service's internal
/// tracking.
#[tokio::test]
#[serial]
async fn cancel_com_primitive_stops_the_native_broadcast_periodic_message() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let cop_handle = start_broadcast_periodic(&mut client, cll_handle, 0xFF).await;
    wait_for_cop_executing(&mut client, cop_handle).await;
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 1);

    client
        .cancel_com_primitive(vci_service_interface::CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "cancelling a broadcast periodic COP should emit PduCopstCancelled"
    );

    assert_eq!(
        server.backdoor.stop_periodic_count(),
        1,
        "CoptCancel must issue a native PassThruStopPeriodicMsg for the owning COP"
    );
    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        0,
        "the native adapter should no longer have a live periodic message"
    );

    drop(events);
    server.shutdown().await;
}

/// Codex review round 2 (P2, PR #101): a failed native
/// `PassThruStopPeriodicMsg` during `CoptCancel` must not silently report
/// success and lose all tracking -- the RPC returns an error, `GetStatus`
/// keeps reporting the COP `Executing` (the cancel genuinely didn't take
/// effect), and a later retry (once the injected failure is cleared)
/// succeeds and genuinely stops the native periodic message.
#[tokio::test]
#[serial]
async fn cancel_com_primitive_restores_tracking_and_errors_when_the_native_stop_fails() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let cop_handle = start_broadcast_periodic(&mut client, cll_handle, 0xFF).await;
    wait_for_cop_executing(&mut client, cop_handle).await;
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 1);

    server
        .backdoor
        .set_stop_periodic_message_error(Some(j2534_0404::ERR_FAILED as std::os::raw::c_long));

    let err = client
        .cancel_com_primitive(vci_service_interface::CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect_err(
            "cancel_com_primitive must surface a failed native PassThruStopPeriodicMsg as an \
             error, not a silent success",
        );
    assert_eq!(err.code(), tonic::Code::Internal);

    // The cancel genuinely didn't take effect: the native periodic message
    // is still live device-side, and GetStatus still reports Executing --
    // never PduCopstCancelled.
    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        1,
        "a failed native stop must leave the periodic message live device-side"
    );
    let response = client
        .get_status(GetStatusRequest {
            handle: Some(get_status_request::Handle::CopHandle(cop_handle)),
        })
        .await
        .expect("get_status(COP) should succeed")
        .into_inner();
    assert_eq!(
        response.status,
        Some(status_response::Status::CopStatus(
            PduComPrimitiveStatus::PduCopstExecuting as i32
        )),
        "GetStatus must keep reporting Executing after a failed CoptCancel -- the tracking \
         entry must have been restored, not dropped"
    );
    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "a failed native stop must never emit PduCopstCancelled"
    );

    // Clear the injected failure and retry -- the restored tracking entry
    // lets this exact same CoptCancel succeed naturally on the second try.
    server.backdoor.set_stop_periodic_message_error(None);

    client
        .cancel_com_primitive(vci_service_interface::CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancel_com_primitive should succeed once the injected failure is cleared");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "the retried CoptCancel should emit PduCopstCancelled"
    );
    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        0,
        "the native adapter should no longer have a live periodic message after the retry \
         succeeds"
    );

    drop(events);
    server.shutdown().await;
}

/// Decision item 2/Consequences: `DisconnectComLogicalLink` of the owning
/// CLL also stops the live native periodic message -- the ADR-010 leak
/// class this stage deliberately reinstates, closed the same way ADR-010
/// closed it for tester-present's own (since-removed) periodic usage.
#[tokio::test]
#[serial]
async fn disconnect_com_logical_link_stops_the_native_broadcast_periodic_message() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let cop_handle = start_broadcast_periodic(&mut client, cll_handle, 0xFF).await;
    wait_for_cop_executing(&mut client, cop_handle).await;
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 1);

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");

    assert_eq!(
        server.backdoor.stop_periodic_count(),
        1,
        "DisconnectComLogicalLink must issue a native PassThruStopPeriodicMsg for this CLL's \
         own live broadcast periodic message"
    );

    server.shutdown().await;
}

/// Decision item 2/Consequences: `PDU_IOCTL_CLEAR_PERIODIC_MSGS` (a
/// channel-wide administrative action) invalidates a live broadcast periodic
/// message device-side as a side effect -- the owning COP's tracked state
/// must reconcile to `PduCopstFinished` rather than being left claiming a
/// `PeriodicMessageId` the native call just invalidated.
#[tokio::test]
#[serial]
async fn clear_periodic_msgs_reconciles_a_live_broadcast_periodic_cop() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let cop_handle = start_broadcast_periodic(&mut client, cll_handle, 0xFF).await;
    wait_for_cop_executing(&mut client, cop_handle).await;
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 1);

    io_ctl_cll(
        &mut client,
        cll_handle,
        j2534_0404::CLEAR_PERIODIC_MSGS,
        None,
        false,
    )
    .await
    .expect("io_ctl(CLEAR_PERIODIC_MSGS) should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLEAR_PERIODIC_MSGS should finalize the owning COP to PduCopstFinished, since the \
         native call already invalidated its periodic message device-side"
    );

    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        0,
        "the native periodic message should be gone after CLEAR_PERIODIC_MSGS"
    );

    drop(events);
    server.shutdown().await;
}

/// Consequences: a broadcast frame's TX-side echo (`[address] ++ payload`,
/// address in the broadcast range) does not match either of Stage 7a's two
/// established-connection routing tiers and must be classified and dropped
/// explicitly, not misrouted to a sibling CLL sharing the same physical
/// channel -- the same leak class ADR-188's Fixes N/P already closed twice
/// for the connection-bound case (see `events_rx_routing.rs`'s
/// `UniqueRespIdKey::matched` for the unit-level coincidence coverage; this
/// exercises the same classification end-to-end).
#[tokio::test]
#[serial]
async fn broadcast_echo_is_dropped_not_misrouted_to_a_sibling_established_connection() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let cll_b = create_and_connect_tp20_cll(&mut client, 0x0322, 2).await;

    for &cll_handle in &[cll_a, cll_b] {
        let mut events = client
            .subscribe_event(vci_service_interface::SubscribeEventRequest {
                handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("subscribe_event should succeed")
            .into_inner();
        start_comm(&mut client, cll_handle)
            .await
            .expect("start_com_primitive(CoptStartcomm) should succeed");
        assert!(
            wait_for_event(&mut events, 2000, |item| matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ))
            .await,
            "each CLL's own CoptStartcomm should independently finish"
        );
    }

    arm_receive_only_monitor(&mut client, cll_a).await;
    arm_receive_only_monitor(&mut client, cll_b).await;

    // A TX-side broadcast echo: [address] ++ payload, address in the
    // broadcast range -- this shape can never legitimately arise from either
    // sibling's own established connection (their TX-IDs are real 11/29-bit
    // CAN identifiers, top byte always < 0xF0).
    let echo = vec![0xF3, 0xAA, 0xBB, 0xCC];
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &echo,
        j2534_0404::PROTOCOL_TP2_0_PS,
        0x0000_0001, // TX_MSG_TYPE
    );

    assert_no_result_data(
        &mut client,
        cll_a,
        "a broadcast echo must never be delivered to CLL A",
    )
    .await;
    assert_no_result_data(
        &mut client,
        cll_b,
        "a broadcast echo must never be misrouted to sibling CLL B",
    )
    .await;

    // Codex review round 2 (ADR-192/Phase 7 Stage 7c fix): the EXACT
    // minimum-size broadcast echo (`rpc_primitive.rs`'s own `3..=8` composed-
    // size range for a broadcast send) -- `[address, payload, payload]`, only
    // 3 bytes. This is the case that used to bypass RX routing entirely: a
    // <4-byte frame produces `frame_can_id == None` (`events.rs`'s own
    // `data.len() >= 4` gate), which `route_frame`'s `None => Some(0)`
    // fallback delivered unconditionally to every CLL, since the old
    // per-entry classifier in `events_rx_routing.rs::UniqueRespIdKey::matched`
    // was never even reached for a frame that short.
    let min_size_echo = vec![0xF4, 0xAA, 0x55];
    server.backdoor.inject_rx_with_status(
        MOCK_CHANNEL_ID,
        &min_size_echo,
        j2534_0404::PROTOCOL_TP2_0_PS,
        0x0000_0001, // TX_MSG_TYPE
    );

    assert_no_result_data(
        &mut client,
        cll_a,
        "a minimum-size (3-byte) broadcast echo must never be delivered to CLL A",
    )
    .await;
    assert_no_result_data(
        &mut client,
        cll_b,
        "a minimum-size (3-byte) broadcast echo must never be misrouted to sibling CLL B",
    )
    .await;

    server.shutdown().await;
}

/// ADR-192 Consequences ("a broadcast periodic re-trigger start rejects
/// synchronously rather than queuing when this CLL's TX dispatch is
/// currently suspended"): a sibling CLL sharing the same physical channel
/// holding `LOCK_PHYSICAL_TX_QUEUE` (ISO 22900-2 §9.4.13.3 use case 1's
/// suspension source) must reject a cyclic broadcast `CoptSendrecv`
/// (`num_send_cycles == -1`) with `FailedPrecondition`, synchronously,
/// before any native `PassThruStartPeriodicMsg` call -- unlike an ordinary
/// transmitting ComPrimitive, which is accepted and queued in `tx_held`
/// instead. Mirrors `locks_and_param_classes.rs`'s
/// `newly_connected_cll_starts_suspended_when_a_sibling_already_holds_the_
/// tx_queue_lock`'s sibling-lock setup shape, adapted to TP2.0's own
/// shared-physical-channel CLL pair (`create_and_connect_tp20_cll`).
#[tokio::test]
#[serial]
async fn broadcast_periodic_rejects_when_a_sibling_holds_the_physical_tx_queue_lock() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("lock_resource should succeed");

    // cll_b joins cll_a's already-open TP2.0 physical channel AFTER the lock
    // is granted -- mirrors the CAN sibling-lock test's own ordering.
    let cll_b = create_and_connect_tp20_cll(&mut client, 0x0322, 2).await;

    set_com_param_unum32(&mut client, cll_b, CP_TP20_BROADCAST_ADDRESS, 0xFF).await;
    let err = start_send_recv(
        &mut client,
        cll_b,
        vec![0x01, 0x02],
        50, // periodic rate, ms
        -1,
        0,
        1, // temp_param_update
        Vec::new(),
    )
    .await
    .expect_err(
        "a broadcast periodic CoptSendrecv must be rejected while a sibling CLL holds \
         LOCK_PHYSICAL_TX_QUEUE on the shared physical channel",
    );
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    assert_eq!(
        server.backdoor.start_periodic_count(),
        0,
        "a rejected broadcast periodic start must never issue a native \
         PassThruStartPeriodicMsg call"
    );
    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        0,
        "the native adapter must have no live periodic message after the rejection"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "a rejected broadcast periodic start must never put traffic on the wire"
    );

    client
        .unlock_resource(UnlockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect("unlock_resource should succeed");

    server.shutdown().await;
}

/// Companion to the sibling-lock test above: the SAME rejection, but driven
/// by this CLL's OWN `PDU_IOCTL_SUSPEND_TX_QUEUE` -- no sibling CLL needed.
/// Mirrors `coptsendrecv_queued_behind_a_racing_stopcomm_teardown_is_
/// cancelled_not_sent_stale`'s own `PDU_IOCTL_SUSPEND_TX_QUEUE` technique.
#[tokio::test]
#[serial]
async fn broadcast_periodic_rejects_when_this_cll_own_tx_queue_is_suspended() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, suspend_id, None, false)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    set_com_param_unum32(&mut client, cll_handle, CP_TP20_BROADCAST_ADDRESS, 0xFF).await;
    let err = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x01, 0x02],
        50, // periodic rate, ms
        -1,
        0,
        1, // temp_param_update
        Vec::new(),
    )
    .await
    .expect_err(
        "a broadcast periodic CoptSendrecv must be rejected while this CLL's own TX queue is \
         suspended (PDU_IOCTL_SUSPEND_TX_QUEUE)",
    );
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    assert_eq!(
        server.backdoor.start_periodic_count(),
        0,
        "a rejected broadcast periodic start must never issue a native \
         PassThruStartPeriodicMsg call"
    );
    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        0,
        "the native adapter must have no live periodic message after the rejection"
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "a rejected broadcast periodic start must never put traffic on the wire"
    );

    server.shutdown().await;
}

/// Codex review round 8 Finding 2 (PR #101, ADR-192/Phase 7 Stage 7c): pins
/// the intentional classification decision -- UNLIKE the two sibling-lock
/// rejection tests above (`LOCK_PHYSICAL_TX_QUEUE`/`PDU_IOCTL_SUSPEND_TX_
/// QUEUE`), a sibling CLL holding `LOCK_PHYSICAL_COM_PARAMS` must NOT block
/// a temp-bound (`temp_param_update=1`) broadcast-periodic start on this
/// CLL, and must never generate a `PduErrEvtRscLocked` for it:
/// `CP_TP20BroadcastInterval` is deliberately classified pacing-class, like
/// `CP_Cs`/`CONFIG_J1939_BRDCST_MIN_DELAY`, never `PDU_PC_BUSTYPE` (see
/// `comparam_support.rs`'s own regression-fence tests, and this ADR's
/// Decision item 3) -- `apply_bustype_lock`'s exclusion/
/// `PduErrEvtRscLocked` machinery lives only in `handle_update_param`'s
/// `CoptUpdateparam` execution path, which this synchronous
/// `StartComPrimitive` bracket never goes through at all. This is an
/// accepted residual the ADR documents explicitly, not a silent gap: a
/// sibling can still move the standing `CONFIG_TP2_0_T_BR_INT` value
/// underneath this CLL regardless of the lock -- the same behavior every
/// other non-BUSTYPE SET_CONFIG-forwarded param already has under ADR-110's
/// "non-conflicting params are still pushed even when locked" design.
#[tokio::test]
#[serial]
async fn broadcast_periodic_temp_bound_start_succeeds_despite_sibling_holding_com_param_lock() {
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    // Both CLLs connect (and each self-promotes via `create_and_connect_
    // tp20_cll`'s own `promote_via_update_param` call) BEFORE the lock is
    // granted, unlike the two sibling-lock rejection tests above -- if
    // cll_b instead joined AFTER cll_a's lock, its own joining-CLL promote
    // would have its BUSTYPE-class keys excluded from promotion by the very
    // lock this test means to isolate (ADR-110's `apply_bustype_lock`),
    // leaving cll_b's OWN Working/Active mismatched on an unrelated BUSTYPE
    // param (e.g. `BIT_SAMPLE_POINT`) and tripping the unrelated
    // `PDU_ERR_TEMPPARAM_NOT_ALLOWED` enqueue-time guard before this test
    // ever reaches the mechanism under test. Granting the lock only after
    // both CLLs are already fully promoted isolates the one thing this test
    // means to exercise: does a temp-bound broadcast-periodic START, issued
    // by cll_b while cll_a holds the lock, get blocked by it?
    let cll_a = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let cll_b = create_and_connect_tp20_cll(&mut client, 0x0322, 2).await;
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect("lock_resource should succeed");

    let cop_handle = start_broadcast_periodic(&mut client, cll_b, 0xFF).await;
    wait_for_cop_executing(&mut client, cop_handle).await;

    assert_eq!(
        server.backdoor.start_periodic_count(),
        1,
        "the temp-bound broadcast periodic start must succeed despite a sibling CLL holding \
         LOCK_PHYSICAL_COM_PARAMS on the shared physical channel"
    );

    // The whole apply/start/revert bracket completes synchronously, inside
    // the StartComPrimitive RPC itself, before that RPC ever returns
    // (ADR-192 Decision item 2) -- so if a PduErrEvtRscLocked were
    // (incorrectly) generated for this, it would already be queued by the
    // time `start_broadcast_periodic` above returned. A plain
    // GetEventItem drain, with no wait/timeout, is therefore sufficient
    // here -- there is no race to account for.
    let mut saw_rsc_locked = false;
    loop {
        let response = client
            .get_event_item(vci_service_interface::GetEventItemRequest {
                handle: Some(
                    vci_service_interface::get_event_item_request::Handle::CllHandle(cll_b),
                ),
            })
            .await
            .expect("get_event_item should succeed")
            .into_inner();
        let Some(item) = response.event_item else {
            break;
        };
        if matches!(
            item.data,
            Some(event_item::Data::ErrorData(error))
                if error == PduErrorEvent::PduErrEvtRscLocked as i32
        ) {
            saw_rsc_locked = true;
        }
    }
    assert!(
        !saw_rsc_locked,
        "a temp-bound broadcast-periodic start must never generate PduErrEvtRscLocked -- \
         CP_TP20BroadcastInterval is deliberately not PDU_PC_BUSTYPE class"
    );

    client
        .unlock_resource(UnlockResourceRequest {
            cll_handle: Some(cll_a),
            lock_mask: LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect("unlock_resource should succeed");

    server.shutdown().await;
}

/// Fix 1 (edge-case-hunter follow-up, ADR-192/Phase 7 Stage 7c): a second
/// concurrent cyclic broadcast `CoptSendrecv` on a CLL that already has a
/// live broadcast periodic message must be rejected synchronously
/// (`FailedPrecondition`), before any second native
/// `PassThruStartPeriodicMsg` call -- otherwise it would silently clobber
/// `LogicalLinkState::tp20_broadcast_periodic`, orphaning the first COP's
/// native periodic message (nothing could `CoptCancel`/tear it down anymore
/// since `tp20_broadcast_periodic` would only name the second COP) and
/// leaking the first `cop_handle` in `primitives` forever.
///
/// Codex review fix (P1, PR #101): also the regression test for the
/// SEQUENTIAL half of that same clobbering hazard's TOCTOU-race variant --
/// `rpc_start_com_primitive` now checks-and-reserves the slot (a
/// `None`-sentinel `Tp20BroadcastPeriodic`) in a single
/// critical section instead of two separate lock acquisitions, so two
/// concurrent starts can no longer both observe "not active" before either
/// one's native call returns. This test proves the reservation pattern did
/// not change the already-tested sequential rejection behavior; it does NOT
/// independently reproduce the genuine concurrent race (two `StartComPrimitive`
/// calls truly interleaved around the native `await`) -- no deterministic
/// hook exists in this harness to pause `start_periodic_message` mid-flight,
/// so that race is closed by inspection/code review of the single critical
/// section, not by a test.
#[tokio::test]
#[serial]
async fn second_concurrent_broadcast_periodic_start_is_rejected_not_clobbered() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;

    let first_cop = start_broadcast_periodic(&mut client, cll_handle, 0xFF).await;
    wait_for_cop_executing(&mut client, first_cop).await;
    assert_eq!(server.backdoor.start_periodic_count(), 1);
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 1);

    set_com_param_unum32(&mut client, cll_handle, CP_TP20_BROADCAST_ADDRESS, 0xF0).await;
    let err = start_send_recv(
        &mut client,
        cll_handle,
        vec![0x03, 0x04],
        50, // periodic rate, ms
        -1,
        0,
        1, // temp_param_update
        Vec::new(),
    )
    .await
    .expect_err(
        "a second concurrent broadcast periodic start must be rejected while the first is \
         still live on this ComLogicalLink",
    );
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    // The rejection must not have touched the native surface at all, and the
    // first COP's own tracking must be untouched -- proven below by
    // successfully cancelling it (which requires `tp20_broadcast_periodic`
    // to still name the first COP, not have been clobbered).
    assert_eq!(
        server.backdoor.start_periodic_count(),
        1,
        "the rejected second start must never issue a native PassThruStartPeriodicMsg call"
    );
    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        1,
        "the first COP's own live periodic message must be unaffected by the rejection"
    );

    client
        .cancel_com_primitive(vci_service_interface::CancelComPrimitiveRequest {
            cop_handle: Some(first_cop),
        })
        .await
        .expect(
            "the first COP must still be reachable/cancellable, proving \
             tp20_broadcast_periodic was never clobbered by the rejected second start",
        );
    assert_eq!(server.backdoor.stop_periodic_count(), 1);
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 0);

    server.shutdown().await;
}

/// Codex review fix (P1, PR #101, ADR-192/Phase 7 Stage 7c): `LockResource
/// (LOCK_PHYSICAL_TX_QUEUE)` must reject while a SIBLING CLL sharing the same
/// physical channel has a live TP2.0 broadcast periodic message -- that
/// message is exactly the same category as a live repeat-message slot or an
/// executing, transmitting COP (both already checked here), an autonomous,
/// device-driven TX stream that never touches `executing_cop`/`primitives`.
/// Without this check, a CLL could acquire the lock while broadcast frames
/// kept transmitting on the resource it believes it now has exclusive access
/// to.
#[tokio::test]
#[serial]
async fn lock_physical_tx_queue_rejects_while_a_sibling_has_a_live_broadcast_periodic_message() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let first_cop = start_broadcast_periodic(&mut client, cll_a, 0xFF).await;
    wait_for_cop_executing(&mut client, first_cop).await;
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 1);

    // cll_b joins cll_a's already-open TP2.0 physical channel AFTER the
    // broadcast periodic message is live -- mirrors
    // `broadcast_periodic_rejects_when_a_sibling_holds_the_physical_tx_queue_
    // lock`'s own sibling-join ordering.
    let cll_b = create_and_connect_tp20_cll(&mut client, 0x0322, 2).await;

    let err = client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_b),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect_err(
            "LockResource(LOCK_PHYSICAL_TX_QUEUE) must be rejected while a sibling CLL has a \
             live TP2.0 broadcast periodic message on the shared physical resource",
        );
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    client
        .cancel_com_primitive(vci_service_interface::CancelComPrimitiveRequest {
            cop_handle: Some(first_cop),
        })
        .await
        .expect("cancelling the broadcast periodic COP should succeed");

    server.shutdown().await;
}

/// Companion to the sibling case above: per ISO 22900-2 Section 9.4.13.2 b)'s
/// active-transmissions clause having no "other" qualifier (the same reading
/// `find_physical_lock_holder`'s Finding J/repeat-message-slot Finding L
/// precedent already applies to this file's own live-COP and repeat-slot
/// checks), a CLL's OWN live broadcast periodic message must also block its
/// own `LockResource(LOCK_PHYSICAL_TX_QUEUE)` attempt -- no
/// `other_handle == handle` self-skip.
#[tokio::test]
#[serial]
async fn lock_physical_tx_queue_rejects_for_this_clls_own_live_broadcast_periodic_message() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let cop_handle = start_broadcast_periodic(&mut client, cll_handle, 0xFF).await;
    wait_for_cop_executing(&mut client, cop_handle).await;
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 1);

    let err = client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_handle),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect_err(
            "LockResource(LOCK_PHYSICAL_TX_QUEUE) must be rejected for a CLL's own live TP2.0 \
             broadcast periodic message -- no other-CLL self-skip",
        );
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    client
        .cancel_com_primitive(vci_service_interface::CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect("cancelling the broadcast periodic COP should succeed");

    server.shutdown().await;
}

/// Fix 2 (edge-case-hunter follow-up, ADR-192/Phase 7 Stage 7c):
/// `PDU_IOCTL_CLEAR_TX_QUEUE` must not mark a live TP2.0 broadcast periodic
/// COP as `cancelled_cops` -- its native `PassThruStartPeriodicMsg` message
/// is still actually transmitting broadcast frames on the wire, so
/// `GetStatus` reporting `PduCopstCancelled` for it would be a lying status.
/// Mirrors the existing `executing_cop`/`detached_tier2_cops` exclusions
/// `ioctl_clear_tx_queue` already applies to this same sweep.
#[tokio::test]
#[serial]
async fn clear_tx_queue_does_not_mark_a_live_broadcast_periodic_cop_cancelled() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let cop_handle = start_broadcast_periodic(&mut client, cll_handle, 0xFF).await;
    wait_for_cop_executing(&mut client, cop_handle).await;
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 1);

    let clear_tx_queue_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_CLEAR_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, clear_tx_queue_id, None, false)
        .await
        .expect("PDU_IOCTL_CLEAR_TX_QUEUE should succeed");

    let status = client
        .get_status(GetStatusRequest {
            handle: Some(get_status_request::Handle::CopHandle(cop_handle)),
        })
        .await
        .expect("get_status(COP) should succeed")
        .into_inner()
        .status;
    assert_eq!(
        status,
        Some(status_response::Status::CopStatus(
            PduComPrimitiveStatus::PduCopstExecuting as i32
        )),
        "PDU_IOCTL_CLEAR_TX_QUEUE must not mark a live broadcast periodic COP cancelled"
    );

    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        1,
        "the native periodic message must still be live (untouched) after CLEAR_TX_QUEUE"
    );

    server.shutdown().await;
}

/// Fix 2 companion (edge-case-hunter follow-up, ADR-192/Phase 7 Stage 7c):
/// the same exclusion, but via `CoptStopcomm`'s own "cancel all queued
/// primitives" sweep rather than `PDU_IOCTL_CLEAR_TX_QUEUE`.
#[tokio::test]
#[serial]
async fn coptstopcomm_cancel_sweep_does_not_mark_a_live_broadcast_periodic_cop_cancelled() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    // CoptStopcomm requires comm_started -- establish the connection first.
    start_comm(&mut client, cll_handle)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish"
    );

    let periodic_cop = start_broadcast_periodic(&mut client, cll_handle, 0xFF).await;
    wait_for_cop_executing(&mut client, periodic_cop).await;
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 1);

    // Empty cop_data: no final message, so CoptStopcomm's own dispatch is a
    // no-op transmit -- only its cancel-all sweep and teardown matter here.
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

    // The cancel-all sweep runs synchronously inside CoptStopcomm's own RPC
    // handler, before it returns -- no need to wait for anything else.
    let status = client
        .get_status(GetStatusRequest {
            handle: Some(get_status_request::Handle::CopHandle(periodic_cop)),
        })
        .await
        .expect("get_status(COP) should succeed")
        .into_inner()
        .status;
    assert_eq!(
        status,
        Some(status_response::Status::CopStatus(
            PduComPrimitiveStatus::PduCopstExecuting as i32
        )),
        "CoptStopcomm's cancel-all sweep must not mark a live broadcast periodic COP cancelled"
    );

    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        1,
        "the native periodic message must still be live (untouched) after CoptStopcomm's \
         cancel-all sweep"
    );

    drop(events);
    server.shutdown().await;
}

/// Fix A (design-advisor consult, Codex review round 3, ADR-192 Decision
/// item 2): `PDU_IOCTL_SUSPEND_TX_QUEUE` must terminate an already-running
/// broadcast periodic, not just reject a future start -- the COP transitions
/// to `PduCopstFinished` (an administrative-action side effect, mirroring
/// `CLEAR_PERIODIC_MSGS`'s own reconciliation shape, NOT `PduCopstCancelled`)
/// and the native periodic message is genuinely stopped device-side.
#[tokio::test]
#[serial]
async fn suspend_tx_queue_terminates_a_live_broadcast_periodic() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let cop_handle = start_broadcast_periodic(&mut client, cll_handle, 0xFF).await;
    wait_for_cop_executing(&mut client, cop_handle).await;
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 1);

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, suspend_id, None, false)
        .await
        .expect("PDU_IOCTL_SUSPEND_TX_QUEUE should succeed");

    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "TX-dispatch suspension must terminate a live broadcast periodic COP with \
         PduCopstFinished -- not leave it Executing, and not falsely PduCopstCancelled"
    );
    assert!(
        !wait_for_event(&mut events, 200, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "suspension terminating a broadcast periodic must never emit PduCopstCancelled"
    );

    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        0,
        "the native periodic message must be stopped device-side once suspension terminates \
         the owning COP"
    );

    drop(events);
    server.shutdown().await;
}

/// Fix A companion: a failed native `PassThruStopPeriodicMsg` triggered by
/// suspension must not silently report success -- the tracking entry is
/// restored onto the CLL (mirroring `CoptCancel`'s own restore-on-failure)
/// so the periodic stays tracked/retryable, and the periodic message stays
/// live device-side. Design choice (documented here and in
/// `terminate_tp20_broadcast_periodic_for_suspension`'s own doc comment,
/// `rpc_misc.rs`): unlike `CoptCancel`'s own RPC, which surfaces a failed
/// stop as a gRPC error, `PDU_IOCTL_SUSPEND_TX_QUEUE` itself still reports
/// success -- the suspension flag change is real and unconditional
/// regardless of this best-effort side effect's own outcome (matching how
/// other best-effort teardown side effects in this codebase, e.g. client
/// filter/repeat-message stop failures during CLL teardown, are logged
/// rather than surfaced as the primary call's own failure).
#[tokio::test]
#[serial]
async fn suspend_tx_queue_retains_tracking_when_the_native_stop_fails() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let cop_handle = start_broadcast_periodic(&mut client, cll_handle, 0xFF).await;
    wait_for_cop_executing(&mut client, cop_handle).await;
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 1);

    server
        .backdoor
        .set_stop_periodic_message_error(Some(j2534_0404::ERR_FAILED as std::os::raw::c_long));

    let suspend_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SUSPEND_TX_QUEUE").await;
    io_ctl_cll(&mut client, cll_handle, suspend_id, None, false)
        .await
        .expect(
            "PDU_IOCTL_SUSPEND_TX_QUEUE is a best-effort trigger for this side effect and must \
             still report success even though the native PassThruStopPeriodicMsg it triggered \
             failed",
        );

    assert!(
        !wait_for_event(&mut events, 300, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
                    || status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "a failed native stop must never finalize the COP -- the terminate genuinely didn't \
         take effect"
    );
    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        1,
        "a failed native stop must leave the periodic message live device-side"
    );
    let status = client
        .get_status(GetStatusRequest {
            handle: Some(get_status_request::Handle::CopHandle(cop_handle)),
        })
        .await
        .expect("get_status(COP) should succeed")
        .into_inner()
        .status;
    assert_eq!(
        status,
        Some(status_response::Status::CopStatus(
            PduComPrimitiveStatus::PduCopstExecuting as i32
        )),
        "GetStatus must keep reporting Executing -- the tracking entry must have been restored \
         onto the CLL, not dropped"
    );

    // Clear the injected failure and retry via CoptCancel -- the restored
    // tracking entry proves the periodic is still tracked/retryable.
    server.backdoor.set_stop_periodic_message_error(None);
    client
        .cancel_com_primitive(vci_service_interface::CancelComPrimitiveRequest {
            cop_handle: Some(cop_handle),
        })
        .await
        .expect(
            "CancelComPrimitive should succeed once the injected failure clears, proving the \
             periodic stayed tracked after the suspension's own failed stop attempt",
        );
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstCancelled as i32
        ))
        .await,
        "the retried CoptCancel should emit PduCopstCancelled"
    );
    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        0,
        "the native adapter should no longer have a live periodic message after the retry \
         succeeds"
    );

    drop(events);
    server.shutdown().await;
}

/// Fix B (design-advisor consult, Codex review round 3, ADR-192 Decision
/// item 2): a shared-channel `DisconnectComLogicalLink` whose native
/// `PassThruStopPeriodicMsg` fails must (a) still succeed client-side, (b)
/// leak-track the `PeriodicMessageId` onto the physical channel's own
/// `SharedChannel::leaked_periodic_message_ids` so a sibling CLL's
/// `LOCK_PHYSICAL_TX_QUEUE` attempt is still rejected, and (c) let
/// `CLEAR_PERIODIC_MSGS` (issued by the sibling, since the owning CLL is
/// gone) clear the leaked tracking outright, after which a LATER lock
/// attempt succeeds.
#[tokio::test]
#[serial]
async fn disconnect_leaks_a_broadcast_periodic_on_a_shared_channel_when_the_native_stop_fails() {
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    let cop_handle = start_broadcast_periodic(&mut client, cll_a, 0xFF).await;
    wait_for_cop_executing(&mut client, cop_handle).await;
    assert_eq!(server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID), 1);

    // cll_b joins cll_a's already-open TP2.0 physical channel, so it stays
    // open once cll_a disconnects below.
    let cll_b = create_and_connect_tp20_cll(&mut client, 0x0322, 2).await;

    server
        .backdoor
        .set_stop_periodic_message_error(Some(j2534_0404::ERR_FAILED as std::os::raw::c_long));

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect(
            "DisconnectComLogicalLink must still succeed client-side even though the \
             best-effort native PassThruStopPeriodicMsg it triggered failed",
        );

    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        1,
        "the native periodic message must still be live device-side after the failed stop"
    );

    // (b) the leaked id keeps blocking a fresh LOCK_PHYSICAL_TX_QUEUE grant
    // from the sibling that kept the channel open.
    let err = client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_b),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect_err(
            "a broadcast periodic leaked by a failed shared-channel-teardown stop must still \
             block a sibling's LOCK_PHYSICAL_TX_QUEUE grant",
        );
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    // (c) CLEAR_PERIODIC_MSGS, issued by the sibling since the owning CLL is
    // gone, clears the leaked tracking outright (the channel-wide native
    // clear already stopped everything on the channel, live or leaked).
    io_ctl_cll(
        &mut client,
        cll_b,
        j2534_0404::CLEAR_PERIODIC_MSGS,
        None,
        false,
    )
    .await
    .expect("io_ctl(CLEAR_PERIODIC_MSGS) should succeed");
    assert_eq!(
        server.backdoor.periodic_msg_count(MOCK_CHANNEL_ID),
        0,
        "CLEAR_PERIODIC_MSGS should have stopped the leaked periodic message device-side"
    );

    // A LATER lock attempt now succeeds, proving the leaked tracking was
    // actually cleared, not merely masked.
    client
        .lock_resource(LockResourceRequest {
            cll_handle: Some(cll_b),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE,
        })
        .await
        .expect(
            "LOCK_PHYSICAL_TX_QUEUE should succeed once CLEAR_PERIODIC_MSGS has cleared the \
             leaked periodic tracking",
        );

    server.shutdown().await;
}

// Fix B companion (`events::handle_channel_hard_error` pushing a live
// broadcast periodic onto a dead `SharedChannel` entry's own
// `leaked_periodic_message_ids`) is NOT covered here: a hard channel error
// unconditionally marks the whole module `PduModstNotAvail`
// (`handle_channel_hard_error`'s own unconditional `state.status =
// PduModstNotAvail`), sticky (ADR-131/ADR-134) until an explicit
// `ModuleDisconnect` -- which `rpc_module_disconnect` implements by
// unconditionally `links.clear()`-ing the ENTIRE `logical_links` map. There
// is no way through this crate's gRPC surface to hard-error one physical
// channel and then, from a FRESH CLL, exercise `LOCK_PHYSICAL_TX_QUEUE`
// against the now-dead `SharedChannel` entry without an intervening
// `ModuleDisconnect` that tears the whole scenario down first -- the same
// class of harness limit `hard_error_then_reconnect_on_the_same_channel_is_
// reported_as_module_not_avail` (above) documents for the neighboring
// `tp20_connection` reset. Covered instead by
// `events::hard_error_broadcast_periodic_tests::hard_channel_error_leaks_a_
// live_broadcast_periodic_onto_the_dead_shared_channel` (`events.rs`), which
// drives `handle_channel_hard_error` directly against hand-built state --
// mirroring `rpc_link.rs::tests::finalize_connected_link_resets_a_stale_
// tp20_connection`'s own resolution for the identical class of limit.

// ── ADR-210: SAE J2534-2 clause 7 Additional Channels for TP2.0 ────────────

/// Mirrors `j1708.rs`'s own
/// `j1708_ch1_establishes_independently_alongside_a_j1708_ps_sibling`
/// (ADR-209)/`honda_diagh.rs`'s/`uart_echo_byte.rs`'s/`gm_uart.rs`'s/
/// `j1939.rs`'s own equivalents: a directly-named `PROTOCOL_TP2_0_CH1` id
/// establishes its own physical channel independently of a
/// `PROTOCOL_TP2_0_PS` sibling on the same module -- confirms
/// `resources::chx_block_base`'s new
/// `PROTOCOL_TP2_0_PS => Some(PROTOCOL_TP2_0_CH1)` entry resolves end-to-end.
/// Unlike every prior family in this series, TP2.0 has a genuine
/// connection-oriented lifecycle, so this test goes further than the prior
/// families' own equivalents: it drives `CoptStartcomm` to a real
/// established connection and verifies the established TX-ID actually
/// prefixes a `CoptSendrecv` payload -- proving `handle_start_comm`'s own
/// TP2.0 entry arm (ADR-210 Decision item 12, without which `CoptStartcomm`
/// would never even initiate a connection request for a `_CHx`-connected
/// link), the `CONNECTION_ESTABLISHED` indication-frame router (same
/// Decision item, keyed on the raw `frame_protocol_id` an incoming native
/// frame reports), and `events_rx_routing.rs`'s `unique_resp_ids`
/// construction (Decision item 13) all correctly recognize a
/// `_CHx`-connected TP2.0 link, not just its `_PS` sibling.
#[tokio::test]
#[serial]
async fn chx_id_establishes_a_real_connection_and_frames_sendrecv_with_the_established_tx_id() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let ps_cll = create_and_connect_tp20_cll(&mut client, 0x0321, 1).await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_TP2_0_PS
    );

    let chx_cll = create_and_connect_tp20_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_TP2_0_CH1,
        &[],
        0x0322,
        2,
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the _PS sibling and its own _CH1 Additional Channel should open two distinct physical \
         channels"
    );
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID + 1),
        j2534_0404::PROTOCOL_TP2_0_CH1,
        "the second connect should open a native PROTOCOL_TP2_0_CH1 channel"
    );

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(chx_cll)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    start_comm(&mut client, chx_cll)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed");
    assert!(
        wait_for_event(&mut events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CoptStartcomm should finish (PduCopstFinished) once the _CHx-connected link's own \
         connection establishes -- this is the exact outcome `handle_start_comm`'s own TP2.0 \
         entry arm and the CONNECTION_ESTABLISHED indication-frame router must both recognize \
         a _CHx id for (ADR-210 Decision item 12)"
    );
    drop(events);

    // TX framing: the established TX-ID (the mock's own `rx_id | 0x1000_0000`
    // assignment) must prefix the payload on the wire -- proves
    // `events_rx_routing.rs`'s `unique_resp_ids` construction gave this
    // _CHx-connected CLL its own real routing entry (ADR-210 Decision
    // item 13), not the deliberate unmatchable sentinel a pre-fix _CHx link
    // would have gotten.
    send_data(&mut client, chx_cll, vec![0xAA, 0xBB, 0xCC], vec![]).await;
    let written = server.backdoor.written_data(MOCK_CHANNEL_ID + 1, 0);
    assert_eq!(
        written,
        vec![0x10, 0x00, 0x03, 0x22, 0xAA, 0xBB, 0xCC],
        "the established TX-ID (0x1000_0322) should prefix the CoptSendrecv payload for the \
         _CHx-connected link"
    );

    let _ = ps_cll;
    server.shutdown().await;
}

/// Regression test: before the mechanical extension of ADR-211's/ADR-212's
/// established pattern (`resources.rs::chx_device_info_supported_
/// parameter` gaining a TP2.0 guard arm), this function returned `None`
/// for every TP2.0 id, so `check_chx_capacity`'s Discovery-cache precheck
/// silently no-opped for this family -- the SAE J2534-2 clause 7 `_CHx`
/// channel-count cap was enforced only by `j2534-0404-mock`'s own
/// independent, generic native-`PassThruConnect`-time check (shared by
/// every in-scope `_CHx` family via the same `chx_capacity` override,
/// predating this fix), not by the Discovery precheck this fix adds.
/// Confirmed genuinely discriminating by hand-reverting the new guard arm:
/// the connect is still rejected either way (this mock's native capacity
/// check runs before its `connect_count` counter increments, so
/// `connect_count` alone cannot distinguish the two paths for this family
/// -- unlike FT-CAN/SW-CAN, which have their own dedicated override
/// breaking that symmetry, per `fault_tolerant_can.rs::connect_rejects_a_
/// chx_index_within_the_generic_capacity_but_above_ft_cans_own`), but the
/// reported gRPC `Code` differs: `InvalidArgument` from the Discovery
/// precheck's synchronous rejection with the fix, vs. a generic
/// native-error-mapped code without it. Simplified relative to that FT-CAN
/// test since TP2.0 shares the generic `chx_capacity` override (no
/// per-family override is needed -- this family is independently
/// self-identifying, unlike FT-CAN/SW-CAN, which collapse onto the generic
/// CAN/ISO15765 base via `base_protocol_id`). The `_CHx` capacity precheck
/// runs before any TP2.0-specific mandatory-ComParam validation
/// (`rpc_link.rs::rpc_connect_com_logical_link`'s `check_chx_capacity`
/// call precedes both `enforce_discovery_capability` and the native
/// connect), so this test does not need to stage TP2.0's five mandatory
/// `PARAM_TP20_*` ComParams before attempting the connect.
#[tokio::test]
#[serial]
async fn connect_rejects_a_chx_index_above_the_cached_capacity() {
    let server = start_j2534_2_server().await;
    server.backdoor.set_chx_capacity(1);
    let mut client = server.client().await;

    let cll_handle = create_tp20_cll(
        &mut client,
        j2534_0404::PROTOCOL_TP2_0_CH1 + 1, // _CH2
        &[],
        1,
    )
    .await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err("_CH2 exceeds the cached capacity (1) -- the connect must be rejected");
    assert_eq!(
        status.code(),
        tonic::Code::InvalidArgument,
        "must be rejected by check_chx_capacity's Discovery precheck (Code::InvalidArgument), \
         not fall through to this mock's own independent native-level _CHx capacity check (a \
         different, non-InvalidArgument code) -- if this fires, chx_device_info_supported_\
         parameter returned None for TP2.0 (the bug this fix corrects)"
    );
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "connect_count stays 0 regardless of which check rejected the connect -- this mock's own \
         native _CHx capacity check (shared chx_capacity override, present independently of this \
         fix) also runs before its connect counter increments, so this assertion alone does not \
         discriminate the fix; status.code() above is the actual discriminator"
    );

    server.shutdown().await;
}

/// Mirrors `j1708.rs`'s own `j1708_connecting_via_compound_chx_name_succeeds`
/// (ADR-209)/`honda_diagh.rs`'s/`uart_echo_byte.rs`'s own equivalents:
/// connecting via the compound `_CHx`-suffixed `protocol_name` grammar
/// ("SAE_J2819_TP2_0_CH1", the resource-table row's own `protocol_name`
/// "SAE_J2819_TP2_0" -- `resources.rs` row 0x025F -- with a `_CH1` suffix)
/// succeeds the same way the raw-id route above does -- this is the test
/// that specifically proves the `names.rs`
/// `requested_index.is_some()` bypass fix (ADR-210 Decision item 5) works:
/// without it, this connect would fail with the clause-6/clause-7
/// mutual-exclusion rejection instead (the TP2.0 arm's own
/// dlc_pin_data-is-empty branch would otherwise reject a resource-table-row-
/// less `_CHx` connect for not selecting the one pin pair clause 19.2.2
/// documents, a pin concept a `_CHx` id doesn't even have).
#[tokio::test]
#[serial]
async fn chx_connecting_via_compound_name_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolName(
                        "SAE_J2819_TP2_0_CH1".to_string(),
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
        .expect(
            "connect_com_logical_link should succeed for the compound _CH1-suffixed name, not \
             be rejected as combining Pin Selection with an Additional Channel",
        );

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_TP2_0_CH1,
        "the compound-name route should open a native PROTOCOL_TP2_0_CH1 channel, same as the \
         raw protocol-id route"
    );

    server.shutdown().await;
}

/// The highest-severity regression test in this ADR: proves
/// `DestroyComLogicalLink`'s own active-connection-teardown gate
/// (`rpc_link.rs`, ADR-210 Decision item 11) actually tears down a
/// `_CHx`-connected TP2.0 link's native connection slot on destroy, instead
/// of leaking it. Before the fix, that gate was keyed on the narrow
/// `is_tp2_0_protocol_id` (an exact `_PS`-only match), so a
/// `_CHx`-connected link's `hw_protocol_id` (`PROTOCOL_TP2_0_CH1`, not
/// `PROTOCOL_TP2_0_PS`) would never satisfy it -- `DestroyComLogicalLink`
/// would tear down the CLL's own service-side bookkeeping but never issue
/// the native `IOCTL_TEARDOWN_CONNECTION` call, permanently occupying one of
/// the mock's four native connection slots on the physical channel with no
/// way to free it.
///
/// **Discriminating construction** (mirrors `connection_rejected_when_all_
/// four_slots_are_full`'s own four-slots-exactly technique, and
/// `destroy_com_logical_link_disarms_the_passive_listener`'s own
/// destroy-then-reuse-and-check-for-PduErrEvtRscLocked introspection, the
/// established mechanism this test file already uses to observe native
/// connection-slot state from outside the mock -- there is no direct
/// backdoor accessor for it): four `_CHx`-connected TP2.0 CLLs sharing one
/// physical channel each establish a real connection, filling all four
/// native slots. One of them is then destroyed. A FIFTH `_CHx`-connected CLL
/// (a fresh `rx_id_proposal`, distinct from all four) then attempts to
/// establish: if the destroyed CLL's own native slot was actually freed
/// (the fix), this fifth connection succeeds; if it leaked (the pre-fix
/// bug), the mock's own already-tested four-slot cap rejects it with
/// `PduErrEvtRscLocked` exactly as `connection_rejected_when_all_four_slots_
/// are_full` demonstrates for a full-and-genuinely-occupied set.
#[tokio::test]
#[serial]
async fn chx_destroy_com_logical_link_tears_down_the_native_connection_and_frees_its_slot() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let mut cll_handles = Vec::new();
    for (i, rx_id) in [0x0200u32, 0x0201, 0x0202, 0x0203].into_iter().enumerate() {
        let cll_handle = create_and_connect_tp20_cll_for_protocol_id_and_pins(
            &mut client,
            j2534_0404::PROTOCOL_TP2_0_CH1,
            &[],
            rx_id,
            i as u64 + 1,
        )
        .await;
        cll_handles.push(cll_handle);
    }
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "all four CLLs should share one physical channel (same _CHx protocol id, same empty \
         pins)"
    );
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_TP2_0_CH1,
        "this test must actually exercise a _CHx-connected physical channel, not silently fall \
         back to _PS"
    );

    for &cll_handle in &cll_handles {
        let mut events = client
            .subscribe_event(vci_service_interface::SubscribeEventRequest {
                handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("subscribe_event should succeed")
            .into_inner();
        start_comm(&mut client, cll_handle)
            .await
            .expect("start_com_primitive(CoptStartcomm) should succeed");
        assert!(
            wait_for_event(&mut events, 2000, |item| matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ))
            .await,
            "each of the four _CHx-connected CLLs should establish, filling all four native \
             connection slots"
        );
    }

    // Destroy one established _CHx-connected CLL. The channel must survive
    // (three siblings still hold it open), exercising the best-effort
    // teardown gate rather than the "channel itself is closing" path.
    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_handles[0]),
        })
        .await
        .expect("destroy_com_logical_link should succeed");
    assert_eq!(
        server.backdoor.disconnect_count(),
        0,
        "the physical channel must survive the destroyed CLL's own destroy -- the three \
         surviving siblings still hold it open, so this test exercises the best-effort \
         IOCTL_TEARDOWN_CONNECTION gate, not a physical-channel-level disconnect"
    );

    // Give the destroy path's own best-effort IOCTL_TEARDOWN_CONNECTION call
    // and its resulting (unconditionally-quarantined) CONNECTION_LOST
    // indication one full poll cycle to drain naturally -- mirrors
    // `destroy_com_logical_link_disarms_the_passive_listener`'s identical
    // wait.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // A fifth _CHx-connected CLL, with a fresh rx_id, now attempts to
    // establish. This succeeds only if the destroyed CLL's own native slot
    // was actually freed.
    let fifth = create_and_connect_tp20_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_TP2_0_CH1,
        &[],
        0x0204,
        5,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(fifth)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, fifth)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(finished, "the fifth CoptStartcomm should finish (not hang)");
    assert!(
        !saw_rsc_locked,
        "the fifth _CHx-connected CLL should be able to establish a new connection after the \
         destroyed sibling's own native slot was freed -- a bare PduCopstFinished alone does not \
         distinguish genuine success from a rejected-but-completed attempt, so this explicitly \
         checks for the ABSENCE of PduErrEvtRscLocked. Before the ADR-210 rpc_link.rs fix, this \
         assertion would fail: the destroyed CLL's own native slot would still be occupied \
         (never torn down, since `is_tp2_0_protocol_id` never matched its _CHx hw_protocol_id), \
         so all four slots would still read as full and this fifth attempt would be rejected \
         exactly like `connection_rejected_when_all_four_slots_are_full`'s own fifth CLL"
    );
    drop(events);

    server.shutdown().await;
}

/// The `DisconnectComLogicalLink` analog of
/// `chx_destroy_com_logical_link_tears_down_the_native_connection_and_frees_
/// its_slot` above: proves `DisconnectComLogicalLink`'s own, SEPARATE
/// active-connection-teardown gate (`rpc_link.rs`, ADR-210 Decision item 11)
/// actually tears down a `_CHx`-connected TP2.0 link's native connection slot
/// on disconnect, instead of leaking it. Before the fix, this gate -- paired
/// with, but distinct from, `DestroyComLogicalLink`'s own -- was likewise
/// keyed on the narrow `is_tp2_0_protocol_id` (an exact `_PS`-only match), so
/// a `_CHx`-connected link's `hw_protocol_id` (`PROTOCOL_TP2_0_CH1`, not
/// `PROTOCOL_TP2_0_PS`) would never satisfy it -- `DisconnectComLogicalLink`
/// would tear down the CLL's own service-side connection bookkeeping but
/// never issue the native `IOCTL_TEARDOWN_CONNECTION` call, permanently
/// occupying one of the mock's four native connection slots on the physical
/// channel with no way to free it.
///
/// Identical discriminating construction to the `DestroyComLogicalLink`
/// precedent, substituting `disconnect_com_logical_link` for
/// `destroy_com_logical_link` on the CLL under test: four `_CHx`-connected
/// TP2.0 CLLs sharing one physical channel each establish a real connection,
/// filling all four native slots. One of them is then DISCONNECTED (not
/// destroyed). A FIFTH `_CHx`-connected CLL (a fresh `rx_id_proposal`,
/// distinct from all four) then attempts to establish: if the disconnected
/// CLL's own native slot was actually freed (the fix), this fifth connection
/// succeeds; if it leaked (the pre-fix bug), the mock's own already-tested
/// four-slot cap rejects it with `PduErrEvtRscLocked`.
#[tokio::test]
#[serial]
async fn chx_disconnect_com_logical_link_tears_down_the_native_connection_and_frees_its_slot() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let mut cll_handles = Vec::new();
    for (i, rx_id) in [0x0220u32, 0x0221, 0x0222, 0x0223].into_iter().enumerate() {
        let cll_handle = create_and_connect_tp20_cll_for_protocol_id_and_pins(
            &mut client,
            j2534_0404::PROTOCOL_TP2_0_CH1,
            &[],
            rx_id,
            i as u64 + 1,
        )
        .await;
        cll_handles.push(cll_handle);
    }
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "all four CLLs should share one physical channel (same _CHx protocol id, same empty \
         pins)"
    );
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_TP2_0_CH1,
        "this test must actually exercise a _CHx-connected physical channel, not silently fall \
         back to _PS"
    );

    for &cll_handle in &cll_handles {
        let mut events = client
            .subscribe_event(vci_service_interface::SubscribeEventRequest {
                handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
            })
            .await
            .expect("subscribe_event should succeed")
            .into_inner();
        start_comm(&mut client, cll_handle)
            .await
            .expect("start_com_primitive(CoptStartcomm) should succeed");
        assert!(
            wait_for_event(&mut events, 2000, |item| matches!(
                item.data,
                Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
            ))
            .await,
            "each of the four _CHx-connected CLLs should establish, filling all four native \
             connection slots"
        );
    }

    // Disconnect one established _CHx-connected CLL (not destroy). The
    // channel must survive (three siblings still hold it open), exercising
    // `DisconnectComLogicalLink`'s own best-effort teardown gate rather than
    // the "channel itself is closing" path.
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handles[0]),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");
    assert_eq!(
        server.backdoor.disconnect_count(),
        0,
        "the physical channel must survive the disconnected CLL's own disconnect -- the three \
         surviving siblings still hold it open, so this test exercises the best-effort \
         IOCTL_TEARDOWN_CONNECTION gate, not a physical-channel-level disconnect"
    );

    // Give the disconnect path's own best-effort IOCTL_TEARDOWN_CONNECTION
    // call and its resulting (unconditionally-quarantined) CONNECTION_LOST
    // indication one full poll cycle to drain naturally.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // A fifth _CHx-connected CLL, with a fresh rx_id, now attempts to
    // establish. This succeeds only if the disconnected CLL's own native
    // slot was actually freed.
    let fifth = create_and_connect_tp20_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_TP2_0_CH1,
        &[],
        0x0224,
        5,
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(fifth)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, fifth)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(finished, "the fifth CoptStartcomm should finish (not hang)");
    assert!(
        !saw_rsc_locked,
        "the fifth _CHx-connected CLL should be able to establish a new connection after the \
         disconnected sibling's own native slot was freed -- a bare PduCopstFinished alone does \
         not distinguish genuine success from a rejected-but-completed attempt, so this \
         explicitly checks for the ABSENCE of PduErrEvtRscLocked. Before the ADR-210 rpc_link.rs \
         fix, this assertion would fail: the disconnected CLL's own native slot would still be \
         occupied (never torn down, since `is_tp2_0_protocol_id` never matched its _CHx \
         hw_protocol_id), so all four slots would still read as full and this fifth attempt \
         would be rejected exactly like `connection_rejected_when_all_four_slots_are_full`'s own \
         fifth CLL"
    );
    drop(events);

    server.shutdown().await;
}

/// The `DisconnectComLogicalLink` analog of
/// `chx_destroy_com_logical_link_disarms_the_passive_listener` above: proves
/// `DisconnectComLogicalLink`'s own, SEPARATE passive-listener-disarm gate
/// (`rpc_link.rs`, ADR-210 Decision item 11) actually clears
/// `SharedChannel::tp20_passive` for a `_CHx`-connected TP2.0 CLL when
/// disconnected (not destroyed), instead of leaving the passive slot
/// permanently armed. Before the fix, this gate -- paired with, but
/// distinct from, `DestroyComLogicalLink`'s own -- was likewise keyed on the
/// narrow `is_tp2_0_protocol_id` (an exact `_PS`-only match), so a
/// `_CHx`-connected passive CLL's own `hw_protocol_id` (`PROTOCOL_TP2_0_CH1`,
/// not `PROTOCOL_TP2_0_PS`) would never satisfy it -- `DisconnectComLogicalLink`
/// would tear down the CLL's own service-side bookkeeping but never disarm
/// the native passive listener, permanently occupying the channel's one
/// passive slot with no way for a fresh CLL to re-arm on the identical
/// `identifier`/`rx_id_passive`.
///
/// Identical discriminating construction to
/// `chx_destroy_com_logical_link_disarms_the_passive_listener`, substituting
/// `disconnect_com_logical_link` for `destroy_com_logical_link` on CLL A: a
/// `_CHx`-connected keepalive CLL keeps the physical channel's `ref_count`
/// above zero across CLL A's disconnect; CLL A's own passive connection is
/// established for real (not merely armed) before disconnecting; and the
/// proof is a fresh CLL B re-arming on the identical
/// `identifier`/`rx_id_passive` and completing its own `CoptStartcomm`
/// without `PduErrEvtRscLocked`.
#[tokio::test]
#[serial]
async fn chx_disconnect_com_logical_link_disarms_the_passive_listener() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll_keepalive = create_and_connect_tp20_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_TP2_0_CH1,
        &[],
        0x0230,
        0,
    )
    .await;

    let cll_a = create_and_connect_tp20_cll_passive_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_TP2_0_CH1,
        &[],
        0x0290,
        0x0390,
        1,
    )
    .await;
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_TP2_0_CH1,
        "this test must actually exercise a _CHx-connected physical channel, not silently fall \
         back to _PS"
    );
    let mut events_a = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_a)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_a)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");
    assert!(
        wait_for_event(&mut events_a, 500, |item| matches!(
            item.data,
            Some(event_item::Data::CopStatus(status))
                if status == PduComPrimitiveStatus::PduCopstFinished as i32
        ))
        .await,
        "CLL A's own passive arm should complete immediately"
    );

    drop(events_a);

    // Establish it for real before disconnecting -- see
    // `chx_destroy_com_logical_link_disarms_the_passive_listener`'s own doc
    // comment for why a merely-armed (`Listening`) slot would not exercise
    // the bug this test guards against.
    assert!(
        server
            .backdoor
            .inject_tp20_passive_connection(MOCK_CHANNEL_ID, 0x1000_0390),
        "the inbound accept should land while CLL A's own listener is armed"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_a),
        })
        .await
        .expect("disconnect_com_logical_link should succeed");
    assert_eq!(
        server.backdoor.disconnect_count(),
        0,
        "the physical channel must survive CLL A's own disconnect -- the keepalive CLL still \
         holds it open -- this test needs the SAME SharedChannel to still be reachable by CLL B \
         below, so a real leak here would indicate this test's own setup is unintentionally \
         tearing down the physical channel instead of exercising the disarm-on-disconnect path"
    );

    // Give the disconnect path's own best-effort IOCTL_TEARDOWN_CONNECTION
    // call and its resulting (unconditionally-quarantined) CONNECTION_LOST
    // indication one full poll cycle to drain naturally.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // A fresh CLL arming passive with the IDENTICAL identifier/rx_id_passive
    // succeeds only if CLL A's own disconnect actually cleared
    // `SharedChannel::tp20_passive` AND its own quarantine entry
    // subsequently released.
    let cll_b = create_and_connect_tp20_cll_passive_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_TP2_0_CH1,
        &[],
        0x0290,
        0x0390,
        2,
    )
    .await;
    let mut events_b = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_b)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();
    start_comm(&mut client, cll_b)
        .await
        .expect("start_com_primitive(CoptStartcomm) should succeed synchronously");

    let mut finished = false;
    let mut saw_rsc_locked = false;
    wait_for_event(&mut events_b, 2000, |item| {
        match &item.data {
            Some(event_item::Data::ErrorData(error))
                if *error == PduErrorEvent::PduErrEvtRscLocked as i32 =>
            {
                saw_rsc_locked = true;
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32 =>
            {
                finished = true;
            }
            _ => {}
        }
        finished
    })
    .await;
    assert!(
        finished,
        "CLL B's own CoptStartcomm should finish (not hang)"
    );
    assert!(
        !saw_rsc_locked,
        "CLL B should be able to re-arm the passive slot after CLL A's own disconnect released \
         it and its quarantine entry drained -- a bare PduCopstFinished alone does not \
         distinguish genuine success from a rejected-but-completed attempt, so this explicitly \
         checks for the ABSENCE of PduErrEvtRscLocked too. Before the ADR-210 rpc_link.rs fix, \
         this assertion would fail: CLL A's own passive slot would still be armed (never \
         disarmed, since `is_tp2_0_protocol_id` never matched its _CHx hw_protocol_id), so CLL \
         B's own re-arm attempt on the identical identifier/rx_id_passive would be rejected"
    );
    drop(events_b);

    server.shutdown().await;
}

//! End-to-end coverage for SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194,
//! Phase 16): connecting with each `CP_NdisPinOption` pin option and
//! confirming the correct native `PassThruConnect` flags, the native
//! `ERR_NO_CONNECTION_ESTABLISHED` activation-failure mapping to
//! `PDU_ERR_NO_CABLE_DETECTED`, the unconditional (no receive-only
//! exemption, unlike Analog Inputs) COP-type gate, the poll task's
//! `rx_supported`-gated RX pass surviving past connect, `PDU_IOCTL_GET_
//! NDIS_ADAPTER_INFO`'s canned struct output, and the ADR-185 Stage 1
//! Discovery-gated connect rejection. Mirrors `analog_inputs.rs`'s/
//! `gm_uart.rs`'s structure for the connect/COP-gate/Discovery coverage; a
//! few small per-file-local helpers are duplicated the same way those files
//! duplicate their own (this codebase's existing convention for this shape
//! of helper, not shared via `harness.rs`).

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, ExpectedResponseData, IoCtlRequest, ModuleHandle, PduError,
    ResourceData, StartComPrimitiveRequest, create_com_logical_link_request, data_item,
    error_detail_from_status, io_ctl_request, resource_data, vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// Resource id of the SAE J2534-2 clause 24 Ethernet_NDIS row --
/// `resources.rs` row 0x0261, `protocol: ChannelProtocol::ETHERNET_NDIS`,
/// `hw_protocol_override: None` (the raw `PROTOCOL_ETHERNET_NDIS` id IS the
/// identity, no separate base id to override onto).
const ETHERNET_NDIS_RESOURCE_ID: u32 = 0x0261;

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// every Ethernet_NDIS resource row requires this opt-in (`names.rs`'s
/// dedicated arm in `resolve_pin_selection`, mirroring the Analog Inputs/
/// GM UART/every other standalone-protocol opt-in gate; Codex review finding
/// on PR #102, which caught this file using plain `TestServer::start()`'s
/// non-opted-in default module -- the exact gap that finding's own fix
/// closes). Mirrors `gm_uart.rs`'s/`tp20.rs`'s identical helper.
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

fn resource_with_protocol_id(raw_protocol_id: u32) -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![],
        bus_type: None,
        protocol: Some(resource_data::Protocol::ProtocolId(raw_protocol_id)),
    }
}

/// Creates a CLL for the Ethernet_NDIS resource id, optionally stages
/// `CP_NdisPinOption` by its D-PDU shortname when `pin_option` is `Some`,
/// then connects. Returns the CLL handle.
async fn create_and_connect_ndis_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    pin_option: Option<u32>,
) -> ComLogicalLinkHandle {
    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id(ETHERNET_NDIS_RESOURCE_ID),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

    if let Some(value) = pin_option {
        set_com_param_by_name(client, cll_handle, "cp_ndispinoption", value).await;
    }

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    cll_handle
}

async fn resolve_ioctl_id(
    client: &mut VciServiceClient<tonic::transport::Channel>,
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

// ── Item (a): connect + CP_NdisPinOption -> connect flag derivation ────────

/// `CP_NdisPinOption` unset (default `0`, auto) connects with neither
/// `CONNECT_FLAG_NDIS_PINS_OPTION1` nor `_OPTION2` set.
#[tokio::test]
#[serial]
async fn connect_with_default_pin_option_sets_neither_connect_flag() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    create_and_connect_ndis_cll(&mut client, None).await;

    let flags = server.backdoor.connect_flags_log();
    assert_eq!(flags.len(), 1);
    assert_eq!(
        flags[0]
            & (j2534_0404::CONNECT_FLAG_NDIS_PINS_OPTION1
                | j2534_0404::CONNECT_FLAG_NDIS_PINS_OPTION2),
        0,
        "auto (unset CP_NdisPinOption) must set neither pin-option connect flag"
    );

    server.shutdown().await;
}

/// `CP_NdisPinOption = 1` connects with `CONNECT_FLAG_NDIS_PINS_OPTION1`
/// only.
#[tokio::test]
#[serial]
async fn connect_with_pin_option_1_sets_option1_flag() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    create_and_connect_ndis_cll(&mut client, Some(1)).await;

    let flags = server.backdoor.connect_flags_log();
    assert_eq!(flags.len(), 1);
    assert_ne!(flags[0] & j2534_0404::CONNECT_FLAG_NDIS_PINS_OPTION1, 0);
    assert_eq!(flags[0] & j2534_0404::CONNECT_FLAG_NDIS_PINS_OPTION2, 0);

    server.shutdown().await;
}

/// `CP_NdisPinOption = 2` connects with `CONNECT_FLAG_NDIS_PINS_OPTION2`
/// only.
#[tokio::test]
#[serial]
async fn connect_with_pin_option_2_sets_option2_flag() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    create_and_connect_ndis_cll(&mut client, Some(2)).await;

    let flags = server.backdoor.connect_flags_log();
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0] & j2534_0404::CONNECT_FLAG_NDIS_PINS_OPTION1, 0);
    assert_ne!(flags[0] & j2534_0404::CONNECT_FLAG_NDIS_PINS_OPTION2, 0);

    server.shutdown().await;
}

// ── Item (b): native ERR_NO_CONNECTION_ESTABLISHED connect failure ─────────

/// The mock's injected `ERR_NO_CONNECTION_ESTABLISHED` activation failure
/// surfaces as `PDU_ERR_NO_CABLE_DETECTED` (ADR-194 Decision), not the
/// generic `PDU_ERR_FCT_FAILED` catch-all.
#[tokio::test]
#[serial]
async fn connect_failure_maps_no_connection_established_to_no_cable_detected() {
    let server = start_j2534_2_server().await;
    server.backdoor.set_ndis_connect_error(Some(
        j2534_0404::ERR_NO_CONNECTION_ESTABLISHED as std::os::raw::c_long,
    ));
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id(ETHERNET_NDIS_RESOURCE_ID),
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
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err("connect should be rejected by the injected activation failure");

    assert_eq!(status.code(), Code::Internal);
    let detail =
        error_detail_from_status(&status).expect("the rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrNoCableDetected as i32,
        "ERR_NO_CONNECTION_ESTABLISHED should map to PDU_ERR_NO_CABLE_DETECTED"
    );

    server.shutdown().await;
}

// ── Item (c): every COP type is rejected, no receive-only exemption ────────

async fn assert_cop_rejected(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    cop_type: vci_service_interface::ComOperationType,
    cop_ctrl_data: Option<ComPrimitiveCtrlData>,
) {
    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: cop_type as i32,
            cop_data: vec![0x00],
            cop_ctrl_data,
        })
        .await
        .expect_err(&format!(
            "{cop_type:?} should be rejected on an Ethernet_NDIS link"
        ));
    assert_eq!(status.code(), Code::InvalidArgument);
    let detail =
        error_detail_from_status(&status).expect("the rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrIdNotSupported as i32,
        "{cop_type:?} should be rejected PDU_ERR_ID_NOT_SUPPORTED"
    );
}

/// Every ComPrimitive type is rejected on a connected Ethernet_NDIS link,
/// including a receive-only `CoptSendrecv` (`NumSendCycles == 0`) -- the
/// case Analog Inputs' own sibling gate WOULD exempt but this one must not
/// (clause 24 bars reads too, ADR-194 Decision).
#[tokio::test]
#[serial]
async fn every_cop_type_is_rejected_including_receive_only_sendrecv() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_ndis_cll(&mut client, None).await;

    // A transmitting CoptSendrecv.
    assert_cop_rejected(
        &mut client,
        cll_handle,
        vci_service_interface::ComOperationType::CoptSendrecv,
        Some(ComPrimitiveCtrlData {
            time: 0,
            num_send_cycles: 1,
            num_receive_cycles: 0,
            temp_param_update: 0,
            expected_response_array: Vec::new(),
            tx_flag: None,
        }),
    )
    .await;

    // A receive-only CoptSendrecv (NumSendCycles == 0) -- the case Analog
    // Inputs exempts, but Ethernet_NDIS must not.
    assert_cop_rejected(
        &mut client,
        cll_handle,
        vci_service_interface::ComOperationType::CoptSendrecv,
        Some(ComPrimitiveCtrlData {
            time: 0,
            num_send_cycles: 0,
            num_receive_cycles: -1,
            temp_param_update: 0,
            expected_response_array: vec![ExpectedResponseData {
                response_type: 0,
                acceptance_id: 0,
                mask_data: vec![],
                pattern_data: vec![],
                unique_resp_ids: vec![],
            }],
            tx_flag: None,
        }),
    )
    .await;

    // CoptStartcomm.
    assert_cop_rejected(
        &mut client,
        cll_handle,
        vci_service_interface::ComOperationType::CoptStartcomm,
        None,
    )
    .await;

    // CoptStopcomm with non-empty cop_data (transmits).
    assert_cop_rejected(
        &mut client,
        cll_handle,
        vci_service_interface::ComOperationType::CoptStopcomm,
        None,
    )
    .await;

    server.shutdown().await;
}

// ── Item (d): the poll task's rx_supported gate survives past connect ──────

/// The poll task does not tear the channel down after connect -- confirms
/// it survives past several `POLL_INTERVAL_MS` RX-pass ticks (the
/// `rx_supported` gate must skip issuing the native `PassThruReadMsgs` call
/// entirely, not just discard its `ERR_NOT_SUPPORTED` result, or the poll
/// task's hard-read-error-closes-the-channel rule would misfire moments
/// after connect). Proven by confirming an unrelated second CLL can still
/// connect afterward -- if the module had wrongly gone `PduModstNotAvail`,
/// every subsequent connect would be rejected.
#[tokio::test]
#[serial]
async fn poll_task_survives_past_first_rx_pass_tick() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    create_and_connect_ndis_cll(&mut client, None).await;

    // Generous headroom past several 10ms poll ticks (ADR-149 guidance:
    // several multiples of the per-round-trip ceiling).
    tokio::time::sleep(tokio::time::Duration::from_millis(
        GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS * 5,
    ))
    .await;

    let second_cll = create_cll(&mut client, j2534_0404::CAN, 2).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(second_cll),
        })
        .await
        .expect(
            "an unrelated CAN connect should still succeed -- the module must not have gone \
             PduModstNotAvail from a misfired Ethernet_NDIS hard-read-error",
        );

    server.shutdown().await;
}

// ── Item (e): PDU_IOCTL_GET_NDIS_ADAPTER_INFO ───────────────────────────────

/// `PDU_IOCTL_GET_NDIS_ADAPTER_INFO` returns the mock's canned
/// `NDIS_ADAPTER_INFORMATION` struct, decoded from `bytearray_data` per
/// `docs/rpc-api-guide.md`'s documented byte layout.
#[tokio::test]
#[serial]
async fn get_ndis_adapter_info_decodes_the_canned_struct() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_ndis_cll(&mut client, None).await;
    let cmd_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_NDIS_ADAPTER_INFO").await;

    let output = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data: None,
            has_output: true,
        })
        .await
        .expect("PDU_IOCTL_GET_NDIS_ADAPTER_INFO should succeed")
        .into_inner()
        .output_data;

    let bytes = match output.and_then(|d| d.data) {
        Some(data_item::Data::BytearrayData(b)) => b.data,
        other => {
            panic!("PDU_IOCTL_GET_NDIS_ADAPTER_INFO should return BytearrayData, got {other:?}")
        }
    };
    assert_eq!(bytes.len(), 226);

    let adapter_unique_id = &bytes[0..128];
    let adapter_name = &bytes[128..192];
    let status = u32::from_le_bytes(bytes[192..196].try_into().unwrap());
    let mac_address = &bytes[196..202];
    let ipv6_address = &bytes[202..218];
    let ipv4_address = &bytes[218..222];
    let ethernet_pin_config = u32::from_le_bytes(bytes[222..226].try_into().unwrap());

    assert!(adapter_unique_id.starts_with(b"MOCK-NDIS-ADAPTER-0001"));
    assert!(adapter_name.starts_with(b"eth0"));
    assert_eq!(status, 1);
    assert_eq!(mac_address, [0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    assert_eq!(
        ipv6_address,
        [
            0xFE, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01
        ]
    );
    assert_eq!(ipv4_address, [192, 168, 7, 1]);
    assert_eq!(ethernet_pin_config, 1);

    server.shutdown().await;
}

/// Codex review, PR #102 round 9: `EthernetPinConfig` must reflect the CLL's
/// own resolved `CP_NdisPinOption`, not always echo Option 1's value --
/// end-to-end round trip through `SetComParam`/`ConnectComLogicalLink`/
/// `PDU_IOCTL_GET_NDIS_ADAPTER_INFO`, complementing
/// `get_ndis_adapter_info_decodes_the_canned_struct`'s auto/Option-1 case
/// above.
#[tokio::test]
#[serial]
async fn get_ndis_adapter_info_reports_option_2_when_connected_with_option_2() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_ndis_cll(&mut client, Some(2)).await;
    let cmd_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_NDIS_ADAPTER_INFO").await;

    let output = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data: None,
            has_output: true,
        })
        .await
        .expect("PDU_IOCTL_GET_NDIS_ADAPTER_INFO should succeed")
        .into_inner()
        .output_data;

    let bytes = match output.and_then(|d| d.data) {
        Some(data_item::Data::BytearrayData(b)) => b.data,
        other => {
            panic!("PDU_IOCTL_GET_NDIS_ADAPTER_INFO should return BytearrayData, got {other:?}")
        }
    };
    let ethernet_pin_config = u32::from_le_bytes(bytes[222..226].try_into().unwrap());
    assert_eq!(
        ethernet_pin_config, 2,
        "a CLL connected with CP_NdisPinOption = 2 should report EthernetPinConfig = 2"
    );

    server.shutdown().await;
}

// ── Item (f): ADR-185 Stage 1 Discovery-cache connect-path gate ────────────

/// Connecting an Ethernet_NDIS resource fails with the Discovery-driven
/// rejection when the mock reports `DEVICE_INFO_ETHERNET_NDIS_SUPPORTED` as
/// unsupported -- mirrors every other ADR-185 Stage 1 family's own
/// connect-time fail-fast coverage.
#[tokio::test]
#[serial]
async fn connect_fails_when_discovery_reports_ethernet_ndis_unsupported() {
    // `enforce_discovery_capability` is a no-op on a module that has not
    // opted into SAE J2534-2 (clause 5's `"J2534-2:"` `pname` prefix) --
    // mirrors `gm_uart.rs`'s own `start_j2534_2_server` precedent.
    let server = TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await;
    server.backdoor.set_ndis_supported(false);
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id(ETHERNET_NDIS_RESOURCE_ID),
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
            cll_handle: Some(cll_handle),
        })
        .await
        .expect_err(
            "connect should be rejected by the ADR-185 Stage 1 Discovery-cache fail-fast layer \
             when the device does not advertise DEVICE_INFO_ETHERNET_NDIS_SUPPORTED",
        );
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the Discovery precheck must reject before any native PassThruConnect is attempted"
    );

    server.shutdown().await;
}

// ── Item (g): Shared-channel join guard (CP_NdisPinOption, ADR-194) ────────

/// SAE J2534-2 clause 24 Ethernet_NDIS Shared-channel join guard (ADR-194
/// Decision): a second CLL joining the same physical Ethernet_NDIS channel
/// with a DIFFERENT resolved `CP_NdisPinOption` than the channel's creator is
/// rejected `PDU_ERR_FCT_FAILED` -- `ChannelKey` cannot distinguish the two
/// CLLs here since `baud`/`pin_select` are always `0` for this protocol,
/// mirroring ADR-178's own `CP_AnalogSampleRate` join-mismatch precedent.
#[tokio::test]
#[serial]
async fn second_cll_with_different_ndis_pin_option_is_rejected_from_joining() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    create_and_connect_ndis_cll(&mut client, Some(1)).await;

    let second_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id(ETHERNET_NDIS_RESOURCE_ID),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");
    set_com_param_by_name(&mut client, second_handle, "cp_ndispinoption", 2).await;

    let status = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(second_handle),
        })
        .await
        .expect_err(
            "a second CLL requesting a different CP_NdisPinOption on an already-connected \
             Ethernet_NDIS channel must be rejected, not silently joined",
        );

    assert_eq!(status.code(), Code::FailedPrecondition);
    let detail =
        error_detail_from_status(&status).expect("the rejection should carry an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrFctFailed as i32);
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "the rejected join must never issue a second native PassThruConnect"
    );

    server.shutdown().await;
}

/// Companion to the rejection test above: a second CLL requesting the SAME
/// resolved `CP_NdisPinOption` as the channel's creator joins normally (no
/// conflict, since both CLLs agree on the resolved connect flags).
#[tokio::test]
#[serial]
async fn second_cll_with_same_ndis_pin_option_joins_successfully() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    create_and_connect_ndis_cll(&mut client, Some(1)).await;
    create_and_connect_ndis_cll(&mut client, Some(1)).await;

    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "both CLLs agreeing on the same resolved CP_NdisPinOption should share one physical \
         channel, not open a second one"
    );

    server.shutdown().await;
}

// ── Item (h): CP_NdisPinOption SetComParam range validation (ADR-194) ──────

/// `CP_NdisPinOption` rejects any value other than `0` (auto)/`1`/`2` at
/// `SetComParam` time -- an out-of-range value would otherwise silently
/// resolve to auto (neither connect-flag bit) at `ConnectComLogicalLink`
/// time with no error, and `GetComParam` would keep reporting the
/// untranslated out-of-range value back to the client.
#[tokio::test]
#[serial]
async fn set_com_param_ndis_pin_option_rejects_out_of_range_values() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id(ETHERNET_NDIS_RESOURCE_ID),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

    let status = client
        .set_com_param(vci_service_interface::SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(vci_service_interface::ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamName(
                    "cp_ndispinoption".to_owned(),
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(vci_service_interface::param_item::ParamData::Unum32(3)),
            }),
        })
        .await
        .expect_err("CP_NdisPinOption = 3 should be rejected as out of range at SetComParam time");

    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

// ── Item (i): ioctl_get_ndis_adapter_info's own two service-side gates ─────

/// `PDU_IOCTL_GET_NDIS_ADAPTER_INFO` is rejected `PDU_ERR_ID_NOT_SUPPORTED`
/// on a non-Ethernet_NDIS CLL.
#[tokio::test]
#[serial]
async fn get_ndis_adapter_info_rejected_on_non_ethernet_ndis_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, j2534_0404::CAN, 1).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("CAN connect should succeed");

    let cmd_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_NDIS_ADAPTER_INFO").await;

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data: None,
            has_output: true,
        })
        .await
        .expect_err(
            "PDU_IOCTL_GET_NDIS_ADAPTER_INFO should be rejected on a non-Ethernet_NDIS link",
        );

    assert_eq!(status.code(), Code::InvalidArgument);
    let detail =
        error_detail_from_status(&status).expect("the rejection should carry an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrIdNotSupported as i32);

    server.shutdown().await;
}

/// `PDU_IOCTL_GET_NDIS_ADAPTER_INFO` is rejected `PDU_ERR_CLL_NOT_CONNECTED`
/// on an Ethernet_NDIS CLL that has not yet connected -- clause 24's own
/// IOCTL definition requires a live `ChannelID`.
#[tokio::test]
#[serial]
async fn get_ndis_adapter_info_rejected_when_not_connected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id(ETHERNET_NDIS_RESOURCE_ID),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

    let cmd_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_GET_NDIS_ADAPTER_INFO").await;

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data: None,
            has_output: true,
        })
        .await
        .expect_err(
            "PDU_IOCTL_GET_NDIS_ADAPTER_INFO should be rejected before ConnectComLogicalLink",
        );

    assert_eq!(status.code(), Code::FailedPrecondition);
    let detail =
        error_detail_from_status(&status).expect("the rejection should carry an ErrorDetail");
    assert_eq!(detail.pdu_error, PduError::PduErrCllNotConnected as i32);

    server.shutdown().await;
}

// ── Item (j): PDU_IOCTL_START_REPEAT_MESSAGE rejection (Codex review, PR #102) ─

/// `PDU_IOCTL_START_REPEAT_MESSAGE` is rejected `PDU_ERR_ID_NOT_SUPPORTED` on
/// a connected Ethernet_NDIS link -- Repeat Messaging is a device-autonomous
/// TRANSMIT mechanism reached through a separate IOCTL dispatch path
/// (`rpc_misc.rs::ioctl_start_repeat_message`), not through
/// `rpc_start_com_primitive`'s COP gate, so the COP-type rejection above does
/// not by itself close this path. Mirrors `analog_inputs.rs`'s own
/// `start_repeat_message_on_connected_link_is_rejected_with_id_not_supported`,
/// the precedent for this exact class of gap (ADR-177/Phase 15, Codex review
/// finding on PR #66).
#[tokio::test]
#[serial]
async fn start_repeat_message_on_connected_link_is_rejected_with_id_not_supported() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_ndis_cll(&mut client, None).await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(start_id)),
            input_data: Some(vci_service_interface::DataItem {
                data: Some(data_item::Data::BytearrayData(
                    vci_service_interface::IoBytearray {
                        data: pack_repeat_message_setup(1000, 0, &[0x01, 0x02], &[], &[], &[]),
                    },
                )),
            }),
            has_output: true,
        })
        .await
        .expect_err(
            "PDU_IOCTL_START_REPEAT_MESSAGE on a connected Ethernet_NDIS link must be rejected",
        );

    assert_eq!(status.code(), Code::InvalidArgument);
    let detail =
        error_detail_from_status(&status).expect("the rejection should carry an ErrorDetail");
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrIdNotSupported as i32,
        "the rejection should report PDU_ERR_ID_NOT_SUPPORTED"
    );

    server.shutdown().await;
}

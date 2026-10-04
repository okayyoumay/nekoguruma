//! End-to-end coverage for SAE J2534-2 clause 12 UART Echo Byte Protocol
//! (ADR-170, Phase 9): connecting the new standalone `UART_ECHO_BYTE_PS`
//! resource row's mandatory internal `SET_CONFIG(CONFIG_J1962_PINS)` (clause
//! 12.2.2's explicit-pin-only model -- the same "always explicit,
//! never implicit" connect ADR-164's SWCAN precedent established), the row's
//! own pin-7 default, an explicit caller override, the raw-id-without-pins
//! rejection (mirroring ADR-168 Correction 3's FT-CAN precedent), the closed
//! ComParam allowlist (clause 12.3.4.1), and the Repeat Messaging
//! START/QUERY/STOP rejection (clause 14 vs. clause 12.3.3.1). Mirrors
//! `ft_can.rs`'s/`sw_can.rs`'s structure; many helpers are reused from
//! `harness.rs`, with a few small per-file-local ones duplicated the same way
//! `ft_can.rs` duplicates its own (this codebase's existing convention for
//! this shape of helper, not shared via `harness.rs`).
//!
//! Unlike SWCAN/FT-CAN, this protocol has no CAN/ISO15765-family
//! relationship at all -- `ChannelProtocol::UART_ECHO_BYTE_PS` is a wholly
//! new, standalone identity (ADR-170 Context/Decision 1), so there is no
//! `base_protocol_id`/FD-mode-substitution-style regression class to cover
//! here the way `ft_can.rs`/`sw_can.rs` do for their own CAN-family siblings.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DataItem, ExpectedResponseData, GetComParamRequest, ModuleHandle,
    ParamItem, PduError, PduParamClass, PinData, ResourceData, SetComParamRequest,
    StartComPrimitiveRequest, create_com_logical_link_request, data_item, error_detail_from_status,
    get_com_param_request, io_ctl_request, param_item, resource_data, subscribe_event_request,
    vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// Resource id of the SAE J2534-2 clause 12 UART Echo Byte Protocol row --
/// `resources.rs` row 0x023A, `protocol: ChannelProtocol::UART_ECHO_BYTE_PS`,
/// `hw_protocol_override: None` (unlike SWCAN/FT-CAN, there is no separate
/// base id to override onto -- the `_PS` id IS the identity).
const UART_ECHO_BYTE_RESOURCE_ID: u32 = 0x023A;

/// A representative K-line baud rate for this protocol; the mock does not
/// validate baud rate values, so any nonzero value would work.
const UART_ECHO_BYTE_BAUD_RATE: u32 = 10_400;

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// every UART Echo Byte resource row requires this opt-in (`names.rs`'s
/// dedicated arm in `resolve_pin_selection`, mirroring the SWCAN/FTCAN opt-in
/// gates).
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

/// Builds a `RscData` resource selecting `protocol_id` via the raw
/// hardware-protocol-id route, with the given typed `(pin_number,
/// pin_type_name)` pairs as `dlc_pin_data` -- same shape as `ft_can.rs`'s own
/// `resource_with_resource_id_and_pins`, specialized to a raw protocol id
/// (both routes resolve through the same `resolve_protocol_id`/
/// `resolve_pin_selection` path; a resource id and a raw hardware protocol id
/// are both valid `RscData::ProtocolId` inputs).
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

/// Creates (but does not connect) a CLL for `protocol_id`/`pins`, on module 1
/// -- same shape as `ft_can.rs`'s `create_cll_for_resource_id_and_pins`.
async fn create_cll_for_protocol_id_and_pins(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    protocol_id: u32,
    pins: &[(u32, &str)],
    _cll_tag: u64,
) -> ComLogicalLinkHandle {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(vci_service_interface::ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(protocol_id, pins),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}

/// Like [`create_cll_for_protocol_id_and_pins`], but also stages `params` and
/// connects -- same shape as `ft_can.rs`'s
/// `create_and_connect_cll_for_resource_id_and_pins`.
async fn create_and_connect_cll_for_protocol_id_and_pins(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    protocol_id: u32,
    pins: &[(u32, &str)],
    params: &[(u32, u32)],
) -> ComLogicalLinkHandle {
    let cll_handle = create_cll_for_protocol_id_and_pins(client, protocol_id, pins, 1).await;

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

/// Item 1: connecting via the resource id succeeds, resolves and connects
/// with `PROTOCOL_UART_ECHO_BYTE_PS` as the hardware protocol id, and the
/// connect sequence includes the mandatory internal
/// `SET_CONFIG(CONFIG_J1962_PINS)` -- clause 12.2.2's explicit-pin-only
/// model (ADR-170 Decision 2), mirroring ADR-164 Decision 1's
/// identical SWCAN requirement. With no caller-supplied `dlc_pin_data`, the
/// row's own default pin (7) is what gets assigned, packed `0x0000_0700`
/// (SS = 0, no secondary pin).
#[tokio::test]
#[serial]
async fn connecting_via_resource_id_applies_the_default_pin() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        UART_ECHO_BYTE_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
        "connecting resource 0x023A should open a native UART_ECHO_BYTE_PS channel"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0700,
        "an unqualified UART Echo Byte connect (no dlc_pin_data) should assign the row's own \
         default pin (7), packed 0x0000PPSS"
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "CONFIG_J1962_PINS should have been SET_CONFIG'd during connect; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Item 2 (mirrors ADR-168 Correction 3's FT-CAN precedent): naming the raw
/// `UART_ECHO_BYTE_PS` hardware protocol id directly (bypassing the resource
/// table entirely -- `resolve_protocol_id` finds no matching row for this
/// numeric value, so `resource_row` is `None`) with no `dlc_pin_data` must be
/// rejected, not silently connected with the resource-table row's own
/// default pin (`0x0000_0700`) -- that default only reflects a choice this
/// diff's *table row* made, not a choice the caller made; clause 12.2.2
/// itself identifies no default pin at all. Confirms `resolve_pin_selection`
/// needed its own dedicated arm for this protocol (the generic clause-6 `_PS`
/// path does not recognize `UART_ECHO_BYTE_PS` as `_PS` vocabulary at all,
/// since it is outside `ps_protocol_id`'s seven in-scope families, and so
/// would otherwise silently treat a bare raw-id connect as an ordinary
/// default-pins connect).
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_directly_without_pins_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(vci_service_interface::ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
                    )),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "naming the raw UART_ECHO_BYTE_PS id directly with no dlc_pin_data must be \
             rejected, not silently defaulted to the resource-table row's own pin",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("no default pin"),
        "the rejection should explain that clause 12.2.2 has no default pin: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Item 3 (accepted half of Item 2's precedent): connecting the raw
/// `UART_ECHO_BYTE_PS` id directly WITH explicit `dlc_pin_data` succeeds, and
/// the caller-supplied pin (3/HI, packed `0x0000_0300`) is what gets
/// assigned -- not the resource-table row's own default (pin 7,
/// `0x0000_0700`), confirming the raw-id route resolves the caller's own
/// pin choice rather than silently substituting the table row's.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_with_explicit_pins_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
        &[(3, "HI")],
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
        "connecting the raw UART_ECHO_BYTE_PS id directly with explicit pins should still open \
         a native UART_ECHO_BYTE_PS channel"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0300,
        "an explicit pin 3/HI request should override the row's own default pin 7"
    );

    server.shutdown().await;
}

/// Regression test (this PR's brief, generalizing the Honda DIAG-H/SAE J1708
/// `rpc_create_com_logical_link` fallback arms to UART Echo Byte via
/// `resources::bustype_default_name_for_hw_protocol_id`): connecting the raw
/// `UART_ECHO_BYTE_PS` id directly with explicit pins, no caller-supplied
/// `bus_type_name`, and no `SetComParam(DATA_RATE, ...)` staged must still
/// receive clause 12.3.4.1's own 9600bps default -- before this fix, the
/// Working ComParamSet stayed empty on this route and `PassThruConnect`
/// received a native baud rate of 0.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_seeds_the_9600_baud_rate_default() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
        &[(7, "K")],
        &[],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.baud_rate(MOCK_CHANNEL_ID),
        9_600,
        "the raw-id no-resource-row UART Echo Byte connect route must still receive clause \
         12.3.4.1's own 9600bps default, not the empty Working set's default of 0"
    );

    server.shutdown().await;
}

/// Sibling regression test: the same raw `UART_ECHO_BYTE_PS` connect, but
/// WITH a caller-supplied, unrelated, well-formed `bus_type_name`
/// (`RscData.bus_type`/`.protocol` are independent fields with no cross-
/// validation) -- this link's own fixed bustype identity must win, not the
/// mismatched name's own bustype defaults.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_ignores_a_mismatched_bus_type_name() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(vci_service_interface::ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![PinData {
                        dlc_pin_number: 7,
                        dlc_pin_type: Some(
                            vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                "K".to_string(),
                            ),
                        ),
                    }],
                    bus_type: Some(resource_data::BusType::BusTypeName(
                        "ISO_14230_1_UART".to_string(),
                    )),
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
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
        9_600,
        "a mismatched bus_type_name must not override this UART Echo Byte link's own 9600bps \
         default"
    );

    server.shutdown().await;
}

/// Codex review finding, PR #57: `dlc_pin_data` supplying a SECOND (secondary)
/// pin alongside a valid primary must be rejected, not silently packed via
/// the general clause-6 two-pin path into `CONFIG_J1962_PINS` -- clause 12's
/// K-line is single-wire and Row 0x023A defines exactly one pin, with no
/// secondary role to pair a second entry with.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_with_two_pins_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(vci_service_interface::ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(
                    j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
                    &[(7, "K"), (15, "L")],
                ),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "supplying a secondary pin for the single-wire UART_ECHO_BYTE_PS bus must be \
             rejected, not silently packed as a two-pin CONFIG_J1962_PINS value",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("single-wire"),
        "the rejection should explain this protocol has no secondary pin role: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejected connect must never reach the native PassThruConnect call"
    );

    server.shutdown().await;
}

/// Item 4: `SetComParam(CP_Baudrate, ...)` (a universal param) is accepted;
/// `SetComParam`/`GetComParam(CP_TesterPresentSendType, ...)` (not in
/// `is_universal_param`) is rejected with `PDU_ERR_COMPARAM_NOT_SUPPORTED` --
/// clause 12.3.4.1's closed parameter list (ADR-170 Decision 3). This also
/// makes mode-0 periodic tester-present (which relies on
/// `CP_TesterPresentSendType`) unreachable for this protocol by construction,
/// satisfying clause 12.3.3.3 with no separate `PassThruStartPeriodicMsg`
/// runtime guard (ADR-170's rejected alternative).
#[tokio::test]
#[serial]
async fn comparam_allowlist_is_limited_to_universal_params() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        UART_ECHO_BYTE_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;

    // CP_Baudrate (DATA_RATE) is a universal param -- already accepted once
    // via the connect helper above; a post-connect SetComParam still
    // succeeds too.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 5_000).await;

    let set_status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(CP_TESTER_PRESENT_SEND_TYPE)),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Unum32(0)),
            }),
        })
        .await
        .expect_err(
            "CP_TesterPresentSendType should be rejected on a UART Echo Byte link -- it is not \
             in clause 12.3.4.1's closed parameter list",
        );
    assert_eq!(set_status.code(), Code::InvalidArgument);
    let set_detail =
        error_detail_from_status(&set_status).expect("rejection should carry an ErrorDetail");
    assert_eq!(
        set_detail.pdu_error,
        PduError::PduErrComparamNotSupported as i32,
        "the rejection should report PDU_ERR_COMPARAM_NOT_SUPPORTED"
    );

    let get_status = client
        .get_com_param(GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(get_com_param_request::Param::ParamId(
                CP_TESTER_PRESENT_SEND_TYPE,
            )),
        })
        .await
        .expect_err(
            "GetComParam should reject CP_TesterPresentSendType on a UART Echo Byte link too",
        );
    assert_eq!(get_status.code(), Code::InvalidArgument);
    let get_detail =
        error_detail_from_status(&get_status).expect("rejection should carry an ErrorDetail");
    assert_eq!(
        get_detail.pdu_error,
        PduError::PduErrComparamNotSupported as i32,
        "the rejection should report PDU_ERR_COMPARAM_NOT_SUPPORTED"
    );

    server.shutdown().await;
}

// ── ADR-216: UART Echo Byte timing ComParams ────────────────────────────────

/// Item 10 (ADR-216): `SetComParam`/`GetComParam` round-trips a UEB timing
/// ComParam (`CP_UebT1Max`) -- confirms the new `is_ueb_timing_param`
/// allowlist arm and native-millisecond storage (no ISO µs conversion).
#[tokio::test]
#[serial]
async fn ueb_timing_param_round_trips() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        UART_ECHO_BYTE_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;

    set_com_param_unum32(&mut client, cll_handle, CP_UEB_T1_MAX, 250).await;
    let readback = get_com_param_unum32(&mut client, cll_handle, CP_UEB_T1_MAX).await;
    assert_eq!(readback, 250);

    server.shutdown().await;
}

/// Item 11 (ADR-216 Decision item 5): a UEB timing ComParam value above
/// `0xFFFF` is rejected at `SetComParam` -- SAE J2534-2's own declared range
/// for all ten is `0x0000..=0xFFFF`.
#[tokio::test]
#[serial]
async fn ueb_timing_param_range_rejection() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        UART_ECHO_BYTE_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;

    let status = try_set_com_param_unum32(&mut client, cll_handle, CP_UEB_T1_MAX, 0x1_0000)
        .await
        .expect_err("CP_UebT1Max above 0xFFFF must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    set_com_param_unum32(&mut client, cll_handle, CP_UEB_T1_MAX, 0xFFFF).await;

    server.shutdown().await;
}

/// Item 12 (ADR-216 Decision item 4, the specific regression the family-wide
/// `is_uart_echo_byte_family_protocol_id` translation predicate exists to
/// prevent): a UEB timing ComParam actually forwards via `SET_CONFIG` on a
/// `_CHx`-connected UEB link, not just a plain `_PS` one. Reuses the ADR-207
/// `_CHx` connection-setup pattern (`resource_with_protocol_id_and_pins`,
/// `PROTOCOL_ECHO_BYTE_CH1`, no `dlc_pin_data`).
#[tokio::test]
#[serial]
async fn ueb_timing_param_forwards_on_a_chx_connected_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_ECHO_BYTE_CH1,
        &[],
        1,
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, CP_UEB_T1_MAX, 250).await;
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed for a directly-named PROTOCOL_ECHO_BYTE_CH1 id");

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_UEB_T1_MAX),
        250,
        "CONFIG_UEB_T1_MAX should have been SET_CONFIG'd on a _CHx-connected UEB link -- without \
         the family-wide translation predicate, this would silently stay 0"
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_UEB_T1_MAX),
        "CONFIG_UEB_T1_MAX should have been SET_CONFIG'd during connect; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)` -- same per-file local helper shape as
/// `repeat_message.rs`'s/`sw_can.rs`'s own (not shared via `harness.rs`).
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

fn unum32_input(value: u32) -> DataItem {
    DataItem {
        data: Some(data_item::Data::Unum32Value(value)),
    }
}

/// Issues `IoCtl` against a `cll_handle` with an optional `input_data`,
/// returning the raw `Status` -- same shape as `repeat_message.rs`'s own
/// `io_ctl_cll`, specialized to no-output IOCTLs (every command this file
/// exercises expects a rejection before any output would matter).
async fn io_ctl_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
    cll_handle: ComLogicalLinkHandle,
    cmd_id: u32,
    input_data: Option<DataItem>,
) -> Result<(), tonic::Status> {
    client
        .io_ctl(vci_service_interface::IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data,
            has_output: true,
        })
        .await
        .map(|_| ())
}

/// Item 5: `PDU_IOCTL_START_REPEAT_MESSAGE`, `_QUERY_REPEAT_MESSAGE`, and
/// `_STOP_REPEAT_MESSAGE` are all rejected with `PDU_ERR_ID_NOT_SUPPORTED` on
/// a connected UART Echo Byte link -- clause 12.3.3.1 explicitly excludes
/// Repeat Messaging for this protocol (ADR-170 Decision 4), unlike the
/// software-ISO-TP case (ADR-165) which only needed to gate `START`.
/// `QUERY`/`STOP` are exercised with an arbitrary `msg_id` (1) that was never
/// started -- `require_owned_repeat_message`'s own new protocol check runs
/// before its ownership check, so this still exercises the dedicated
/// rejection rather than the (also-correct, but differently-worded)
/// `PDU_ERR_INVALID_MSG_ID` a UART Echo Byte link would otherwise always hit
/// instead (it can never actually own a `msg_id`, since `START` rejects
/// before ever registering one).
#[tokio::test]
#[serial]
async fn repeat_messaging_is_rejected_on_all_three_commands() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        UART_ECHO_BYTE_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let query_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_QUERY_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    let setup_input = DataItem {
        data: Some(data_item::Data::BytearrayData(
            vci_service_interface::IoBytearray {
                data: pack_repeat_message_setup(1000, 0, &[0x01, 0x02], &[], &[], &[]),
            },
        )),
    };
    let start_status = io_ctl_cll(&mut client, cll_handle, start_id, Some(setup_input))
        .await
        .expect_err(
            "PDU_IOCTL_START_REPEAT_MESSAGE must be rejected on a UART Echo Byte link -- clause \
             12.3.3.1 excludes Repeat Messaging for this protocol",
        );
    assert_eq!(start_status.code(), Code::InvalidArgument);
    assert!(
        start_status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"),
        "{}",
        start_status.message()
    );

    let query_status = io_ctl_cll(&mut client, cll_handle, query_id, Some(unum32_input(1)))
        .await
        .expect_err("PDU_IOCTL_QUERY_REPEAT_MESSAGE must be rejected on a UART Echo Byte link");
    assert_eq!(query_status.code(), Code::InvalidArgument);
    assert!(
        query_status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"),
        "{}",
        query_status.message()
    );

    let stop_status = io_ctl_cll(&mut client, cll_handle, stop_id, Some(unum32_input(1)))
        .await
        .expect_err("PDU_IOCTL_STOP_REPEAT_MESSAGE must be rejected on a UART Echo Byte link");
    assert_eq!(stop_status.code(), Code::InvalidArgument);
    assert!(
        stop_status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"),
        "{}",
        stop_status.message()
    );

    server.shutdown().await;
}

/// Item 6 (edge-case-hunter finding 1, post-implementation review): a
/// connected UART Echo Byte link must actually be able to run
/// `FIVE_BAUD_INIT` via `CoptStartcomm` -- clause 12 is a K-line physical
/// layer (ADR-170 Decision 8/Context) with no service-level change needed
/// for `run_protocol_init` itself, but `rpc_primitive.rs`'s `is_kline` gate
/// (which decides whether `select_init_sequence`/`five_baud`/`fast_init` are
/// ever populated at all) originally only matched ISO9141/ISO14230, so this
/// path was entirely unreachable for a UART Echo Byte link before that gate
/// was extended.
///
/// `CP_InitializationSettings` (and every other K-line-only ComParam) is
/// *not* settable on this protocol at all (clause 12.3.4.1's closed
/// parameter list -- `comparam_allowlist_is_limited_to_universal_params`
/// above), so the spec-mandated `CP_InitializationSettings == 1` path can
/// never be reached here; this exercises the only path that remains
/// reachable instead -- the legacy single-cop_data-byte heuristic
/// (`select_init_sequence`'s `None => legacy_heuristic()` branch, whose
/// `init_data.len() == 1` arm does not require a specific protocol id).
///
/// This test fails against the pre-fix `is_kline` gate (`matches!` limited to
/// `ISO9141 | ISO14230`): `call_time_sequence` would be `None` (the `is_kline
/// .then(...)` short-circuits), so `five_baud` would never be populated and
/// `five_baud_init_count()` would stay `0`.
///
/// Uses an explicit `cop_ctrl_data.num_receive_cycles = 1` (rather than the
/// `None` this test originally sent) since the P2 backlog fix
/// (`implementation-notes.md`, PR #58 Codex review finding) made
/// `PROTOCOL_UART_ECHO_BYTE_PS`'s legacy-heuristic branch read
/// `num_receive_cycles` the same way the spec-mandated branch does
/// (`ctrl.map(...).unwrap_or(0)`) -- an absent `cop_ctrl_data` now defaults to
/// `0` (suppress delivery) instead of the old unconditional-delivery
/// behavior, so this "keybytes are delivered" steady-state check needs an
/// explicit `1` to keep testing that path (see
/// `startcomm_num_receive_cycles_zero_suppresses_keybyte_delivery` below for
/// the `0` case, and `startcomm_rejects_invalid_num_receive_cycles` for the
/// out-of-range rejection).
#[tokio::test]
#[serial]
async fn startcomm_runs_five_baud_init_via_the_legacy_single_byte_heuristic() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        UART_ECHO_BYTE_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;

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

    // ADR-115 single-consumer correction: see the identical comment on
    // `iso9141_spec_five_baud_contract_happy_path` in `startcomm_comparam.rs`.
    let result = wait_for_cop_finished_and_result_data(&mut events, 2000).await;

    assert_eq!(
        server.backdoor.five_baud_init_count(),
        1,
        "CoptStartcomm with a single-byte cop_data on a connected UART Echo Byte link should \
         run FIVE_BAUD_INIT via the legacy heuristic -- pre-fix, `is_kline` never matched this \
         protocol, so the init sequence was never even selected"
    );
    assert_eq!(server.backdoor.fast_init_count(), 0);
    assert_eq!(
        server.backdoor.five_baud_init_input(MOCK_CHANNEL_ID),
        Some(0x33),
        "the legacy path must use the raw cop_data[0] byte as the 5-baud address"
    );
    // ADR-075: five-baud keybytes are delivered raw; the safe wrapper
    // (`j2534-0404/src/lib.rs::five_baud_init`) always allocates exactly a
    // 2-byte output buffer, so only the mock's first 2 canned response bytes
    // ever surface, same as every other five-baud happy-path test.
    assert_result_data(&result, &[], &[], &[0x55, 0x8F]);

    drop(events);
    server.shutdown().await;
}

/// P2 backlog fix regression test (`implementation-notes.md`, PR #58 Codex
/// review finding): `cop_ctrl_data.num_receive_cycles = 0` on a UART Echo
/// Byte `CoptStartcomm` (the legacy single-byte 5-baud-init heuristic --
/// `CP_InitializationSettings` is outside this protocol's ComParam allowlist,
/// clause 12.3.4.1, so the spec-mandated branch can never fire here) must
/// suppress ECU key byte delivery, mirroring the spec-mandated branch's own
/// `NumReceiveCycles=0` contract (ADR-076). Pre-fix, this branch hardcoded
/// `deliver_keybytes: true` unconditionally and never read `num_receive_
/// cycles` at all, so key bytes were always delivered regardless of what the
/// client requested; this test fails against that pre-fix behavior (a
/// `ResultData` event would have arrived).
///
/// Codex review finding (PR #80): this test originally dropped the live
/// `SubscribeEvent` stream after observing `Finished` and fell back to a
/// `GetEventItem` poll (`assert_no_result_data`) to prove absence -- but
/// ADR-115's single-consumer rule means a `ResultData` produced while a
/// subscriber is live is delivered on THAT stream, not queued for
/// `GetEventItem`, and `wait_for_event`'s predicate silently discards any
/// notification that doesn't match (here, an out-of-order `ResultData`
/// arriving before `Finished`) -- so the subsequent poll would find nothing
/// even if suppression had regressed, a false pass. Fixed the same way
/// [`startcomm_with_absent_ctrl_data_suppresses_keybyte_delivery_by_default`]
/// below already does: capture any `ResultData` seen on the live stream
/// while waiting for `Finished`, rather than dropping the stream first.
#[tokio::test]
#[serial]
async fn startcomm_num_receive_cycles_zero_suppresses_keybyte_delivery() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        UART_ECHO_BYTE_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;

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

    let mut result_data_seen = None;
    let finished = wait_for_event(&mut events, 2000, |item| {
        if let Some(vci_service_interface::event_item::Data::ResultData(result)) = &item.data {
            result_data_seen = Some(result.clone());
        }
        matches!(
            item.data,
            Some(vci_service_interface::event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        )
    })
    .await;
    assert!(
        finished,
        "CoptStartcomm should finish even though keybyte delivery is suppressed"
    );
    assert!(
        result_data_seen.is_none(),
        "NumReceiveCycles=0 must suppress ECU key byte delivery on a UART Echo Byte link, but a \
         ResultData event arrived: {result_data_seen:?}"
    );

    assert_eq!(
        server.backdoor.five_baud_init_count(),
        1,
        "the 5-baud init should still run with NumReceiveCycles=0"
    );
    assert_eq!(
        server.backdoor.five_baud_init_input(MOCK_CHANNEL_ID),
        Some(0x33),
        "the legacy path must still use the raw cop_data[0] byte as the 5-baud address, even \
         when keybyte delivery is suppressed"
    );

    drop(events);
    server.shutdown().await;
}

/// edge-case-hunter finding (verification pass over the P2 backlog fix
/// above): an ABSENT `cop_ctrl_data` (omitted entirely, not `Some` with an
/// explicit value) on a UART Echo Byte `CoptStartcomm` must also suppress
/// ECU key byte delivery, proving `rpc_primitive.rs`'s
/// `ctrl.map(|c| c.num_receive_cycles).unwrap_or(0)` read -- a real behavior
/// change from the old unconditional-delivery default for this protocol.
/// This test fails if `.unwrap_or(0)` were reverted to the old hardcoded
/// `deliver_keybytes: true` (a `ResultData` event would arrive) or changed
/// to `.unwrap_or(1)` (same outcome).
///
/// Unlike [`startcomm_num_receive_cycles_zero_suppresses_keybyte_delivery`]
/// above, this test cannot drop the live `SubscribeEvent` stream and fall
/// back to a `GetEventItem` poll to prove absence: ADR-115's single-consumer
/// rule means a `ResultData` produced while a subscriber is live is
/// delivered on that stream, not queued for `GetEventItem` -- and if
/// delivery were NOT suppressed, `ResultData` would race with (often
/// precede) the `PduCopstFinished` status on that same stream. A naive
/// `wait_for_event` for `Finished` alone would silently discard an
/// out-of-order `ResultData` (it drops every notification that doesn't
/// match its predicate) before the subsequent `GetEventItem` poll ever ran,
/// producing a false pass against the very regression this test exists to
/// catch. So this captures any `ResultData` seen on the stream while
/// waiting for `Finished`, mirroring `wait_for_cop_finished_and_result_data`'s
/// order-independent capture but asserting the opposite outcome.
#[tokio::test]
#[serial]
async fn startcomm_with_absent_ctrl_data_suppresses_keybyte_delivery_by_default() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        UART_ECHO_BYTE_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;

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

    let mut result_data_seen = None;
    let finished = wait_for_event(&mut events, 2000, |item| {
        if let Some(vci_service_interface::event_item::Data::ResultData(result)) = &item.data {
            result_data_seen = Some(result.clone());
        }
        matches!(
            item.data,
            Some(vci_service_interface::event_item::Data::CopStatus(status))
                if status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
        )
    })
    .await;
    assert!(
        finished,
        "CoptStartcomm should finish even though keybyte delivery is suppressed"
    );
    assert!(
        result_data_seen.is_none(),
        "an absent cop_ctrl_data must default to NumReceiveCycles=0 (suppress ECU key byte \
         delivery) on a UART Echo Byte link, but a ResultData event arrived: \
         {result_data_seen:?}"
    );

    assert_eq!(
        server.backdoor.five_baud_init_count(),
        1,
        "the 5-baud init should still run with an absent cop_ctrl_data"
    );
    assert_eq!(
        server.backdoor.five_baud_init_input(MOCK_CHANNEL_ID),
        Some(0x33),
        "the legacy path must still use the raw cop_data[0] byte as the 5-baud address, even \
         when cop_ctrl_data is absent"
    );

    drop(events);
    server.shutdown().await;
}

/// P2 backlog fix regression test (sibling of the suppression test above): a
/// `num_receive_cycles` value outside `0`/`1` (e.g. `2`) on a UART Echo Byte
/// `CoptStartcomm` must be rejected synchronously with `INVALID_ARGUMENT`,
/// matching the spec-mandated branch's own error contract shape (ADR-076).
/// Pre-fix, this branch never read `num_receive_cycles` at all, so any value
/// was silently accepted (and would have delivered key bytes unconditionally).
#[tokio::test]
#[serial]
async fn startcomm_rejects_invalid_num_receive_cycles() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        UART_ECHO_BYTE_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0x33],
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 2,
                temp_param_update: 0,
                expected_response_array: Vec::<ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await
        .expect_err(
            "num_receive_cycles=2 should be rejected for a UART Echo Byte 5-baud init request",
        );
    assert_eq!(status.code(), Code::InvalidArgument);

    assert_eq!(
        server.backdoor.five_baud_init_count(),
        0,
        "a rejected COP must never reach the adapter"
    );

    server.shutdown().await;
}

/// Item 8 (design-advisor fix, closing two edge-case-hunter findings from a
/// second review pass over Item 6 above): `CoptStartcomm` on a connected
/// UART Echo Byte link with EMPTY `cop_data` must be rejected, not silently
/// treated as "skip init" -- clause 12 defines no init-less start for this
/// protocol. `rpc_primitive.rs`'s dedicated `PROTOCOL_UART_ECHO_BYTE_PS &&
/// cop_data.len() != 1` guard closes this: pre-fix, empty `cop_data` fell
/// through `call_time_sequence` (still `Some(FiveBaud)`, since
/// `legacy_heuristic` matched on protocol id alone even before ADR-170
/// Decision 8) but the `!request.cop_data.is_empty()` condition on the
/// legacy `five_baud` arm (`rpc_primitive.rs`) was false, so `five_baud`
/// resolved to `None` and no init ran at all -- `start_com_primitive` would
/// still have returned `Ok`/finished with zero wire traffic. This test
/// fails against that pre-fix behavior (the call would succeed, not error).
#[tokio::test]
#[serial]
async fn startcomm_with_empty_cop_data_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        UART_ECHO_BYTE_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;

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
            "CoptStartcomm with empty cop_data on a UART Echo Byte link must be rejected -- \
             clause 12 defines no init-less start for this protocol",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("exactly one byte"),
        "unexpected error message: {}",
        status.message()
    );

    assert_eq!(
        server.backdoor.five_baud_init_count(),
        0,
        "the rejected COP must never reach the adapter"
    );
    assert_eq!(server.backdoor.fast_init_count(), 0);

    server.shutdown().await;
}

/// Item 9 (design-advisor fix, sibling of Item 8): `CoptStartcomm` on a
/// connected UART Echo Byte link with an OVERSIZED `cop_data` (>= 4 bytes)
/// must also be rejected, not silently misrouted to native FAST_INIT --
/// clause 12 never defines a fast-init for this protocol.
/// `rpc_primitive.rs`'s dedicated guard closes this: pre-fix, a 4-byte
/// `cop_data` would have made `legacy_heuristic` (pre-ADR-170-Decision-8)
/// select `InitSequence::Fast` for this protocol (its `init_data.len() ==
/// 1` arm requires exactly one byte), and the `is_kline` gate then let a
/// `fast_init` value populate and actually dispatch a native FAST_INIT.
/// This test fails against that pre-fix behavior (the call would succeed
/// and `fast_init_count()` would read back `1`, not `0`).
#[tokio::test]
#[serial]
async fn startcomm_with_oversized_cop_data_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        UART_ECHO_BYTE_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;

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
            "CoptStartcomm with a 4-byte cop_data on a UART Echo Byte link must be rejected -- \
             clause 12 never defines a FAST_INIT for this protocol",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("exactly one byte"),
        "unexpected error message: {}",
        status.message()
    );

    assert_eq!(
        server.backdoor.fast_init_count(),
        0,
        "the rejected COP must never reach the adapter as a native FAST_INIT"
    );
    assert_eq!(server.backdoor.five_baud_init_count(), 0);

    server.shutdown().await;
}

/// Item 7 (edge-case-hunter finding 2, post-implementation review):
/// connecting via the `ProtocolName`/`RscData` route (not `ProtocolId`/
/// `ResourceId`, which items 1-3 above already cover) must still route
/// through `resolve_pin_selection` -- proving the `names.rs` fix renaming
/// (and extending) `matched_row_is_sw_or_ft` to
/// `matched_row_needs_pin_selection`, which now also recognizes a matched
/// UART Echo Byte row (`resources::is_uart_echo_byte_protocol_id`).
///
/// Pre-fix, a table row matched by `ProtocolName("UART_ECHO_BYTE")` (row
/// 0x023A) but not `matched_row_is_sw_or_ft` took the
/// `table_row_matched && !matched_row_is_sw_or_ft` bypass branch, which calls
/// `resolve_channel_selection(..., None, ...)` directly and never calls
/// `resolve_pin_selection` at all -- so `CONFIG_J1962_PINS` would never be
/// `SET_CONFIG`'d during connect and `server.backdoor.config_value(...,
/// CONFIG_J1962_PINS)` would read back `0` (the mock's channel-params-map
/// default for a key never written), not the row's own default pin 7
/// (`0x0000_0700`) -- this test fails against that pre-fix behavior.
#[tokio::test]
#[serial]
async fn connecting_via_protocol_name_applies_the_default_pin() {
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
                        "UART_ECHO_BYTE".to_string(),
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
        UART_ECHO_BYTE_BAUD_RATE,
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
        "connecting via ProtocolName(\"UART_ECHO_BYTE\") should open a native \
         UART_ECHO_BYTE_PS channel, same as the ProtocolId/ResourceId route"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0700,
        "connecting via ProtocolName(\"UART_ECHO_BYTE\") should still apply the row's own \
         default pin (7) via resolve_pin_selection -- pre-fix this route bypassed pin \
         selection entirely"
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "CONFIG_J1962_PINS should have been SET_CONFIG'd during connect; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Regression test (edge-case-hunter close-out finding on PR #63's Honda
/// DIAG-H round-4 fix, `names.rs::row_needs_dynamic_pin_selection`): the same
/// shared predicate that fixed Honda DIAG-H's canonical-name-plus-non-default-pin
/// connect also fixes this identical latent gap for UART Echo Byte -- clause
/// 12.2.2 documents no closed pin set at all (unlike Honda DIAG-H's two-pin
/// or FT-CAN's two-pin-pair sets), so `resolve_pin_selection`'s own arm
/// already accepts ANY single pin the caller supplies, not just row 0x023A's
/// own default (pin 7, the VW/Audi convention). Naming the canonical
/// `protocol_name` ("UART_ECHO_BYTE") with `dlc_pin_data` selecting pin 6
/// (any non-default single pin) must succeed too, not be rejected against
/// the row's own fixed pin data before that dynamic acceptance ever runs.
#[tokio::test]
#[serial]
async fn connecting_via_protocol_name_with_a_non_default_pin_succeeds() {
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
                        dlc_pin_number: 6,
                        dlc_pin_type: Some(
                            vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                                "K".to_string(),
                            ),
                        ),
                    }],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolName(
                        "UART_ECHO_BYTE".to_string(),
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
        UART_ECHO_BYTE_BAUD_RATE,
    )
    .await;

    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0600,
        "the canonical-name route must assign the caller's own explicit pin 6, not reject it \
         against the resource-table row's pin-7 default"
    );

    server.shutdown().await;
}

// ── ADR-207: SAE J2534-2 clause 7 Additional Channels for UART Echo Byte ───

/// Mirrors `gm_uart.rs`'s own
/// `gm_uart_ch1_establishes_independently_alongside_a_gm_uart_ps_sibling`/
/// `j1939.rs`'s own `j1939_ch1_establishes_independently_alongside_a_j1939_ps_sibling`
/// (ADR-206): a directly-named `PROTOCOL_ECHO_BYTE_CH1` id establishes its own
/// physical channel independently of a `PROTOCOL_UART_ECHO_BYTE_PS` sibling on
/// the same module -- confirms `resources::chx_block_base`'s new
/// `PROTOCOL_UART_ECHO_BYTE_PS => Some(PROTOCOL_ECHO_BYTE_CH1)` entry
/// resolves end-to-end, and that a `_CHx` id (no J1962 pin concept at all)
/// connects with no `dlc_pin_data`.
#[tokio::test]
#[serial]
async fn uart_echo_byte_ch1_establishes_independently_alongside_a_uart_echo_byte_ps_sibling() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let ps_cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        UART_ECHO_BYTE_RESOURCE_ID,
        &[(j2534_0404::DATA_RATE, UART_ECHO_BYTE_BAUD_RATE)],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
    );

    let chx_cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(vci_service_interface::ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_ECHO_BYTE_CH1, &[]), // _CH1
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
            cll_handle: Some(chx_cll_handle),
        })
        .await
        .expect(
            "connect_com_logical_link should succeed for a directly-named PROTOCOL_ECHO_BYTE_CH1 \
             id",
        );

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the _PS sibling and its own _CH1 Additional Channel should open two distinct physical \
         channels"
    );
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID + 1),
        j2534_0404::PROTOCOL_ECHO_BYTE_CH1,
        "the second connect should open a native PROTOCOL_ECHO_BYTE_CH1 channel"
    );

    let _ = ps_cll;
    server.shutdown().await;
}

/// Regression test: before the mechanical extension of ADR-211's/ADR-212's
/// established pattern (`resources.rs::chx_device_info_supported_
/// parameter` gaining a UART Echo Byte guard arm), this function returned
/// `None` for every UART Echo Byte id, so `check_chx_capacity`'s
/// Discovery-cache precheck silently no-opped for this family -- the SAE
/// J2534-2 clause 7 `_CHx` channel-count cap was enforced only by
/// `j2534-0404-mock`'s own independent, generic
/// native-`PassThruConnect`-time check (shared by every in-scope `_CHx`
/// family via the same `chx_capacity` override, predating this fix), not
/// by the Discovery precheck this fix adds. Confirmed genuinely
/// discriminating by hand-reverting the new guard arm: the connect is
/// still rejected either way (this mock's native capacity check runs
/// before its `connect_count` counter increments, so `connect_count` alone
/// cannot distinguish the two paths for this family -- unlike FT-CAN/
/// SW-CAN, which have their own dedicated override breaking that symmetry,
/// per `fault_tolerant_can.rs::connect_rejects_a_chx_index_within_the_
/// generic_capacity_but_above_ft_cans_own`), but the reported gRPC `Code`
/// differs: `InvalidArgument` from the Discovery precheck's synchronous
/// rejection with the fix, vs. a generic native-error-mapped code without
/// it. Simplified relative to that FT-CAN test since UART Echo Byte shares
/// the generic `chx_capacity` override (no per-family override is needed
/// -- this family is independently self-identifying, unlike FT-CAN/SW-CAN,
/// which collapse onto the generic CAN/ISO15765 base via
/// `base_protocol_id`).
#[tokio::test]
#[serial]
async fn connect_rejects_a_chx_index_above_the_cached_capacity() {
    let server = start_j2534_2_server().await;
    server.backdoor.set_chx_capacity(1);
    let mut client = server.client().await;

    let cll_handle = create_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_ECHO_BYTE_CH1 + 1, // _CH2
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
        Code::InvalidArgument,
        "must be rejected by check_chx_capacity's Discovery precheck (Code::InvalidArgument), \
         not fall through to this mock's own independent native-level _CHx capacity check (a \
         different, non-InvalidArgument code) -- if this fires, chx_device_info_supported_\
         parameter returned None for UART Echo Byte (the bug this fix corrects)"
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

/// Regression test for the ADR-207 Decision item 10 fix: before it,
/// `rpc_misc.rs`'s Repeat Messaging rejection on all three
/// `PDU_IOCTL_*_REPEAT_MESSAGE` commands was keyed on the arm-gate-only
/// `is_uart_echo_byte_protocol_id` (an exact `_PS` match), so a
/// `_CHx`-connected link -- which cannot autonomously maintain the clause
/// 12.4.2 handshake any more than its `_PS` sibling can -- would have
/// silently skipped this rejection instead. Mirrors
/// `repeat_messaging_is_rejected_on_all_three_commands` above, connected via
/// a directly-named `PROTOCOL_ECHO_BYTE_CH1` id instead of the `_PS`
/// resource-table row.
#[tokio::test]
#[serial]
async fn repeat_messaging_is_rejected_on_all_three_commands_for_a_chx_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(vci_service_interface::ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_ECHO_BYTE_CH1, &[]),
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
        .expect("connect_com_logical_link should succeed for a directly-named PROTOCOL_ECHO_BYTE_CH1 id");

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let query_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_QUERY_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    let setup_input = DataItem {
        data: Some(data_item::Data::BytearrayData(
            vci_service_interface::IoBytearray {
                data: pack_repeat_message_setup(1000, 0, &[0x01, 0x02], &[], &[], &[]),
            },
        )),
    };
    let start_status = io_ctl_cll(&mut client, cll_handle, start_id, Some(setup_input))
        .await
        .expect_err(
            "PDU_IOCTL_START_REPEAT_MESSAGE must be rejected on a _CHx-connected UART Echo Byte \
             link exactly as it is on its _PS sibling -- clause 12.3.3.1's exclusion applies to \
             the whole family, not just the _PS route",
        );
    assert_eq!(start_status.code(), Code::InvalidArgument);
    assert!(
        start_status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"),
        "{}",
        start_status.message()
    );

    let query_status = io_ctl_cll(&mut client, cll_handle, query_id, Some(unum32_input(1)))
        .await
        .expect_err("PDU_IOCTL_QUERY_REPEAT_MESSAGE must be rejected on a _CHx-connected UART Echo Byte link");
    assert_eq!(query_status.code(), Code::InvalidArgument);
    assert!(
        query_status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"),
        "{}",
        query_status.message()
    );

    let stop_status = io_ctl_cll(&mut client, cll_handle, stop_id, Some(unum32_input(1)))
        .await
        .expect_err("PDU_IOCTL_STOP_REPEAT_MESSAGE must be rejected on a _CHx-connected UART Echo Byte link");
    assert_eq!(stop_status.code(), Code::InvalidArgument);
    assert!(
        stop_status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"),
        "{}",
        stop_status.message()
    );

    server.shutdown().await;
}

/// Mirrors `gm_uart.rs`'s own `connecting_via_compound_chx_name_succeeds`/
/// `j1939.rs`'s own equivalent (ADR-206): connecting via the compound
/// `_CHx`-suffixed `protocol_name` grammar ("UART_ECHO_BYTE_CH1") succeeds the
/// same way the raw-id route above does.
#[tokio::test]
#[serial]
async fn connecting_via_compound_chx_name_succeeds() {
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
                        "UART_ECHO_BYTE_CH1".to_string(),
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
        j2534_0404::PROTOCOL_ECHO_BYTE_CH1,
        "the compound-name route should open a native PROTOCOL_ECHO_BYTE_CH1 channel, same as \
         the raw protocol-id route"
    );

    server.shutdown().await;
}

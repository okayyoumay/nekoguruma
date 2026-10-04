//! End-to-end coverage for SAE J2534-2 clause 17 SAE J1708 Protocol
//! (ADR-175, Phase 11): connecting the new standalone `J1708_PS` resource
//! row's mandatory internal `SET_CONFIG(CONFIG_J1962_PINS)` (clause
//! 17.3.2.1's explicit-pin-only model), the row's own pins-3/11
//! default, the OPEN (non-closed-set) pin selection -- unlike SWCAN/FT-CAN/
//! Honda DIAG-H, this protocol accepts any well-formed 1-or-2-pin selection
//! (ADR-175 Decision 3) -- the closed ComParam allowlist (clause
//! 17.3.2.2.1, `DATA_RATE`/`LOOPBACK`/`PARAM_MESSAGE_PRIORITY`), and the new
//! `MSG_PRIORITY_VALUE` TxFlags mechanism (clause 17.4.5, ADR-175 Decision
//! 6). Mirrors `honda_diagh.rs`'s structure for the connect/pin/ComParam
//! coverage; a few small per-file-local helpers are duplicated the same way
//! `honda_diagh.rs`/`uart_echo_byte.rs` duplicate their own (this codebase's
//! existing convention for this shape of helper, not shared via
//! `harness.rs`).
//!
//! Unlike SWCAN/FT-CAN, this protocol has no CAN/ISO15765-family
//! relationship at all -- `ChannelProtocol::J1708_PS` is a wholly new,
//! standalone identity (ADR-175 Context/Decision 1), the same shape as UART
//! Echo Byte (ADR-170) and Honda DIAG-H (ADR-174). Unlike both of those,
//! though, this protocol's pin set is NOT closed (clause 17 documents no
//! valid-pin table on the J1962 connector), and its ComParam allowlist is a
//! superset of the universal `DATA_RATE`/`LOOPBACK` pair, not a narrower
//! subset -- these differences are what this file's own pin/ComParam tests
//! are built around.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, DataItem,
    GetObjectIdRequest, IoBytearray, IoCtlRequest, ModuleHandle, ObjectType, ParamItem, PduError,
    PduParamClass, PinData, ResourceData, SetComParamRequest, create_com_logical_link_request,
    data_item, error_detail_from_status, get_com_param_request, io_ctl_request, param_item,
    resource_data,
};

use crate::harness::*;

/// Resource id of the SAE J2534-2 clause 17 SAE J1708 Protocol row --
/// `resources.rs` row 0x023C, `protocol: ChannelProtocol::J1708_PS`,
/// `hw_protocol_override: None` (unlike SWCAN/FT-CAN, there is no separate
/// base id to override onto -- the `_PS` id IS the identity).
const J1708_RESOURCE_ID: u32 = 0x023C;

/// D-PDU `CP_MessagePriority`'s native `ComParamId` value (0x8083,
/// `service_params::PARAM_MESSAGE_PRIORITY`) -- not re-exported from
/// `j2534_0404`, so named directly here the same way `honda_diagh.rs`'s
/// sibling file names `CP_INITIALIZATION_SETTINGS`.
const CP_MESSAGE_PRIORITY: u32 = 0x8083;

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// every J1708 resource row requires this opt-in (`names.rs`'s dedicated arm
/// in `resolve_pin_selection`, mirroring the SWCAN/FTCAN/UART Echo Byte/
/// Honda DIAG-H opt-in gates).
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

/// Builds a `RscData` resource selecting `protocol_id` via the raw
/// hardware-protocol-id route, with the given typed `(pin_number,
/// pin_type_name)` pairs as `dlc_pin_data` -- same shape as
/// `honda_diagh.rs`'s own `resource_with_protocol_id_and_pins`.
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

/// Like [`create_and_connect_cll_for_module`] (harness.rs), but for the raw
/// protocol-id + explicit-pins route -- same shape as `honda_diagh.rs`'s own
/// `create_and_connect_cll_for_protocol_id_and_pins`.
async fn create_and_connect_cll_for_protocol_id_and_pins(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    protocol_id: u32,
    pins: &[(u32, &str)],
    params: &[(u32, u32)],
) -> ComLogicalLinkHandle {
    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
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
        .expect("cll_handle should be present");

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
/// with `PROTOCOL_J1708_PS` as the hardware protocol id, and the connect
/// sequence includes the mandatory internal `SET_CONFIG(CONFIG_J1962_PINS)`
/// -- clause 17.3.2.1's explicit-pin-only model (ADR-175 Decision
/// 2). With no caller-supplied `dlc_pin_data`, the row's own default pins
/// (3/11, an editorial convenience default with no textual basis in clause
/// 17 or clause 6 -- see `resources.rs`'s own `PINS_SAE_J1708` doc comment)
/// are what get assigned, packed `0x0000_030B`.
#[tokio::test]
#[serial]
async fn connecting_via_resource_id_applies_the_default_pins() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll =
        create_and_connect_cll_for_module(&mut client, MOCK_MODULE_HANDLE, J1708_RESOURCE_ID, &[])
            .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_J1708_PS,
        "connecting resource 0x023C should open a native J1708_PS channel"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_030B,
        "an unqualified J1708 connect (no dlc_pin_data) should assign the row's own default \
         pins (3/11), packed 0x0000PPSS"
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "CONFIG_J1962_PINS should have been SET_CONFIG'd during connect; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Item 2: naming the raw `J1708_PS` hardware protocol id directly
/// (bypassing the resource table entirely) with no `dlc_pin_data` must be
/// rejected, not silently connected with the resource-table row's own
/// default pins -- clause 17.3.2.1 itself identifies no default pin at all.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_directly_without_pins_is_rejected() {
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
                        j2534_0404::PROTOCOL_J1708_PS,
                    )),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "naming the raw J1708_PS id directly with no dlc_pin_data must be rejected, not \
             silently defaulted to the resource-table row's own pins",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("no default pin"),
        "the rejection should explain that clause 17.3.2.1 has no default pin: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Item 3 (ADR-175 Decision 3's key behavioral proof): connecting the raw
/// `J1708_PS` id directly with a single EXPLICIT pin that is NOT one of the
/// resource row's own default pins (pin 1, typed PLUS/primary) succeeds --
/// unlike SWCAN/FT-CAN/Honda DIAG-H, clause 17 documents no closed valid-pin
/// set on the J1962 connector, so any well-formed single pin is accepted.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_with_an_explicit_alternate_single_pin_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_J1708_PS,
        &[(1, "PLUS")],
        &[],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_J1708_PS
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0100,
        "an explicit pin 1 request -- not one of the row's own default pins (3/11) -- should be \
         accepted verbatim, proving there is no closed-set pin rejection for this protocol"
    );

    server.shutdown().await;
}

/// Item 4 (sibling of Item 3): connecting the raw `J1708_PS` id directly
/// with an explicit TWO-pin pair (pins 1/9, typed HI/LOW) that is NOT the
/// row's own default pair (3/11) also succeeds -- clause 17 documents no
/// closed set of valid pin-pairs either, unlike FT-CAN's own two-pin-pair
/// restriction.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_with_an_explicit_alternate_pin_pair_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_J1708_PS,
        &[(1, "HI"), (9, "LOW")],
        &[],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0109,
        "an explicit pin-pair (1/9) -- not the row's own default pair (3/11) -- should be \
         accepted verbatim, proving there is no closed-set pin-pair rejection for this protocol"
    );

    server.shutdown().await;
}

/// Regression test (edge-case-hunter finding, Phase 11 verification):
/// connecting via the raw `J1708_PS` id bypasses the resource table entirely
/// (`resource_row == None`, `bus_type: None` in
/// `resource_with_protocol_id_and_pins`), so `rpc_create_com_logical_link`
/// cannot select `sae_j1708_uart()` via its usual `resource_row`/
/// `bus_type_name` lookup -- without `rpc_link.rs`'s dedicated
/// `is_j1708_protocol_id` fallback (mirroring the identical Honda DIAG-H fix,
/// ADR-174/Codex review PR #63 round 2), the Working set stays empty and
/// `ComParamSet::baud_rate()` feeds `PassThruConnect` a native baud rate of 0
/// instead of clause 17.2.2's minimum-support default of 9600bps. Asserts the
/// native connect actually receives 9600, the same value the resource-id
/// route (Item 1, which already goes through `resource_row.is_some()`) gets
/// for free. Unlike Honda DIAG-H, `DATA_RATE` genuinely is
/// `SetComParam`-configurable for J1708 (a caller has a workaround this test
/// doesn't exercise), but the connect-time default must still be correct
/// without one.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_seeds_the_9600_baud_rate_default() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_J1708_PS,
        &[(1, "PLUS")],
        &[],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.baud_rate(MOCK_CHANNEL_ID),
        9_600,
        "the raw-id no-resource-row connect route must still receive clause 17.2.2's \
         minimum-support 9600bps default, not the empty Working set's default of 0"
    );

    server.shutdown().await;
}

/// Regression test (mirrors Honda DIAG-H's own
/// `connecting_the_raw_protocol_id_ignores_a_mismatched_bus_type_name`,
/// `honda_diagh.rs`): a caller naming `J1708_PS` via `protocol` while ALSO
/// supplying an unrelated, well-formed `bus_type_name` (`RscData.bus_type`/
/// `.protocol` are independent fields with no cross-validation) must not let
/// that mismatched name's own bustype defaults win --
/// `resources::bustype_default_name_for_hw_protocol_id`'s protocol-identity-
/// first check (generalized from the Honda DIAG-H/SAE J1708 fallback arms
/// this diff replaced) makes clause 17.2.2's 9600bps default authoritative
/// for this protocol regardless of whatever `bus_type_name` string RscData
/// happens to carry. Unlike Honda DIAG-H, `DATA_RATE` genuinely is
/// `SetComParam`-configurable for J1708, but the connect-time default itself
/// must still resolve correctly, not fall through to a mismatched
/// `bus_type_name`'s own (10400bps) default.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_ignores_a_mismatched_bus_type_name() {
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
                                "PLUS".to_string(),
                            ),
                        ),
                    }],
                    bus_type: Some(resource_data::BusType::BusTypeName(
                        "ISO_14230_1_UART".to_string(),
                    )),
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_J1708_PS,
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
        "a mismatched bus_type_name must not override this J1708 link's own 9600bps \
         minimum-support default"
    );

    server.shutdown().await;
}

/// Item 5: `SetComParam`/`GetComParam(CP_Baudrate/CP_Loopback/
/// CP_MessagePriority, ...)` (clause 17.3.2.2.1's closed parameter list) are
/// accepted; an unrelated param (`CP_TesterPresentSendType`) is rejected
/// with `PDU_ERR_COMPARAM_NOT_SUPPORTED`. Unlike Honda DIAG-H, `CP_Baudrate`
/// IS accepted here -- clause 17.2.2 gives a minimum-support default, not a
/// fixed value, so `DATA_RATE` stays genuinely `SetComParam`-configurable
/// for this protocol.
#[tokio::test]
#[serial]
async fn comparam_allowlist_accepts_data_rate_loopback_and_message_priority() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle =
        create_and_connect_cll_for_module(&mut client, MOCK_MODULE_HANDLE, J1708_RESOURCE_ID, &[])
            .await;

    // Clause 17.3.2.2.1's own closed parameter list is settable.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 9_600).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::LOOPBACK, 0).await;
    set_com_param_unum32(&mut client, cll_handle, CP_MESSAGE_PRIORITY, 3).await;

    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_MESSAGE_PRIORITY).await,
        3,
        "CP_MessagePriority should round-trip through GetComParam"
    );

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
            "CP_TesterPresentSendType should be rejected on a J1708 link -- it is not in clause \
             17.3.2.2.1's closed parameter list",
        );
    assert_eq!(set_status.code(), Code::InvalidArgument);
    let set_detail = error_detail_from_status(&set_status)
        .expect("CP_TesterPresentSendType rejection should carry an ErrorDetail");
    assert_eq!(
        set_detail.pdu_error,
        PduError::PduErrComparamNotSupported as i32,
        "CP_TesterPresentSendType rejection should report PDU_ERR_COMPARAM_NOT_SUPPORTED"
    );

    let get_status = client
        .get_com_param(vci_service_interface::GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(get_com_param_request::Param::ParamId(
                CP_TESTER_PRESENT_SEND_TYPE,
            )),
        })
        .await
        .expect_err("GetComParam should reject CP_TesterPresentSendType on a J1708 link too");
    assert_eq!(get_status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

/// Item 6 (ADR-175 Decision 6's key behavioral proof): a `CP_MessagePriority`
/// value within clause 17.4.5's 1..=8 range reaches the native `TxFlags`
/// `MSG_PRIORITY_VALUE` field (bits 16-19) verbatim on a `CoptSendrecv` send.
#[tokio::test]
#[serial]
async fn message_priority_in_range_reaches_native_tx_flags() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        J1708_RESOURCE_ID,
        &[(CP_MESSAGE_PRIORITY, 3)],
    )
    .await;

    send_data(&mut client, cll_handle, vec![0x01, 0x02, 0x03], vec![]).await;

    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        tx_flags & j2534_0404::TX_FLAG_MSG_PRIORITY_VALUE,
        3 << 16,
        "an in-range CP_MessagePriority (3) should land in TxFlags bits 16-19 verbatim; got \
         {tx_flags:#010x}"
    );

    server.shutdown().await;
}

/// Item 7 (sibling of Item 6): an OUT-OF-RANGE `CP_MessagePriority` value (9,
/// above clause 17.4.5's 1..=8 range) clamps to 8 (the lowest priority) in
/// native `TxFlags`, per clause 17.4.5's own text.
#[tokio::test]
#[serial]
async fn message_priority_out_of_range_clamps_to_8() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        J1708_RESOURCE_ID,
        &[(CP_MESSAGE_PRIORITY, 9)],
    )
    .await;

    send_data(&mut client, cll_handle, vec![0x01, 0x02, 0x03], vec![]).await;

    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        tx_flags & j2534_0404::TX_FLAG_MSG_PRIORITY_VALUE,
        8 << 16,
        "an out-of-range CP_MessagePriority (9) should clamp to 8 in TxFlags bits 16-19; got \
         {tx_flags:#010x}"
    );

    server.shutdown().await;
}

/// Item 8 (sibling of Item 6/7): an explicit `CP_MessagePriority = 0`
/// clamps to 8, the same "unset default" result clause 17.4.5's own text
/// describes. (Updated per Codex review, PR #64: `sae_j1708_uart()` now
/// seeds `CP_MessagePriority = 8` itself -- see
/// `comparam_defaults.rs::sae_j1708_uart_has_9600bps_default` -- so a truly
/// unset ComParam is no longer reachable through any connect route; this
/// test instead proves the clamp logic directly for the same "unset" value
/// 0 the ComParam would read as before that fix.)
#[tokio::test]
#[serial]
async fn message_priority_absent_clamps_to_8() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        J1708_RESOURCE_ID,
        &[(CP_MESSAGE_PRIORITY, 0)],
    )
    .await;

    send_data(&mut client, cll_handle, vec![0x01, 0x02, 0x03], vec![]).await;

    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        tx_flags & j2534_0404::TX_FLAG_MSG_PRIORITY_VALUE,
        8 << 16,
        "an absent CP_MessagePriority should clamp to 8 in TxFlags bits 16-19, same as an \
         explicit 0 or any other out-of-range value; got {tx_flags:#010x}"
    );

    server.shutdown().await;
}

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id` via
/// `GetObjectId(OBJT_IO_CTRL, ...)` -- same per-file local helper shape as
/// `repeat_message.rs`'s own (not shared via `harness.rs`, matching this
/// codebase's existing convention).
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

/// Issues `PDU_IOCTL_START_REPEAT_MESSAGE` with a minimal `condition = 0`
/// setup (unconditional continued retransmission, ADR-165 Decision 6, no
/// mask/pattern needed for this test) and returns the returned `MsgId`. Same
/// shape as `repeat_message.rs`'s own `start_repeat_message`, condensed to
/// this file's one caller.
async fn start_repeat_message_unconditional(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: ComLogicalLinkHandle,
    cmd_id: u32,
    repeat_msg_data: Vec<u8>,
) -> u32 {
    let output = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
            input_data: Some(DataItem {
                data: Some(data_item::Data::BytearrayData(IoBytearray {
                    // No UniqueRespIdTable is set up in this test (only the
                    // TxFlags composition on RepeatMsgData[0] is under test),
                    // so no expected-response header gets prepended to the
                    // mask/pattern template -- a non-empty mask/pattern must
                    // be supplied directly or the 0-byte template is
                    // rejected by clause 17's own 1..=4095 TX size range.
                    data: pack_repeat_message_setup(
                        1000,
                        0,
                        &repeat_msg_data,
                        &[0xFF],
                        &[0x01],
                        &[],
                    ),
                })),
            }),
            has_output: true,
        })
        .await
        .expect("PDU_IOCTL_START_REPEAT_MESSAGE should succeed")
        .into_inner()
        .output_data;
    match output.and_then(|d| d.data) {
        Some(data_item::Data::Unum32Value(msg_id)) => msg_id,
        other => panic!(
            "PDU_IOCTL_START_REPEAT_MESSAGE should return a Unum32Value MsgId, got {other:?}"
        ),
    }
}

/// Regression test (Codex review, PR #64): `rpc_misc.rs::ioctl_start_
/// repeat_message` composes the actually-transmitted `RepeatMsgData[0]`
/// message's TxFlags independently of `rpc_primitive::apply_resolved_tx_
/// flags` (the same reason it has its own local SW-CAN gate) -- without a
/// matching J1708-gated `msg_priority_tx_flags()` OR there, a J1708 link's
/// `CP_MessagePriority` value would never reach `MSG_PRIORITY_VALUE` on a
/// repeat-messaging transmission, even though clause 17 places no Repeat
/// Messaging exclusion on this protocol (verified separately, Item 5 below
/// -- `IoCtl` itself was never rejected for this protocol; this test proves
/// the TxFlags composition specifically, not just IOCTL acceptance).
#[tokio::test]
#[serial]
async fn start_repeat_message_applies_the_configured_message_priority() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        J1708_RESOURCE_ID,
        &[(CP_MESSAGE_PRIORITY, 3)],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let _msg_id = start_repeat_message_unconditional(
        &mut client,
        cll_handle,
        start_id,
        vec![0x01, 0x02, 0x03],
    )
    .await;

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        tx_flags & j2534_0404::TX_FLAG_MSG_PRIORITY_VALUE,
        3 << 16,
        "an in-range CP_MessagePriority (3) should reach TxFlags bits 16-19 on the actually- \
         transmitted RepeatMsgData[0] message, the same as an ordinary CoptSendrecv send; got \
         {tx_flags:#010x}"
    );

    server.shutdown().await;
}

/// Item 9 (SAE J2534-2 clause 17.4.3 Table 67): the TX message size range's
/// boundaries -- 1 byte and 4095 bytes are both accepted, 4096 bytes is
/// rejected.
#[tokio::test]
#[serial]
async fn tx_message_size_range_boundaries() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle =
        create_and_connect_cll_for_module(&mut client, MOCK_MODULE_HANDLE, J1708_RESOURCE_ID, &[])
            .await;

    send_data(&mut client, cll_handle, vec![0xAA; 1], vec![]).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "a 1-byte message (clause 17.4.3 Table 67's TX minimum) should be accepted"
    );

    send_data(&mut client, cll_handle, vec![0xAA; 4095], vec![]).await;
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "a 4095-byte message (clause 17.4.3 Table 67's TX maximum) should be accepted"
    );

    let status = send_data_expect_rejected(&mut client, cll_handle, vec![0xAA; 4096]).await;
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status
            .message()
            .contains("outside the valid TX message size range (1..=4095 bytes)"),
        "unexpected error message: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        2,
        "the rejected oversized message must never reach the native PassThruWriteMsgs call"
    );

    server.shutdown().await;
}

// ── ADR-209: SAE J2534-2 clause 7 Additional Channels for SAE J1708 ────────

/// Mirrors `honda_diagh.rs`'s own
/// `honda_diagh_ch1_establishes_independently_alongside_a_honda_diagh_ps_sibling`
/// (ADR-208)/`uart_echo_byte.rs`'s/`gm_uart.rs`'s/`j1939.rs`'s own
/// equivalents (ADR-207/ADR-206): a directly-named `PROTOCOL_J1708_CH1` id
/// establishes its own physical channel independently of a
/// `PROTOCOL_J1708_PS` sibling on the same module -- confirms
/// `resources::chx_block_base`'s new
/// `PROTOCOL_J1708_PS => Some(PROTOCOL_J1708_CH1)` entry resolves end-to-end,
/// and that a `_CHx` id (no J1962 pin concept at all) connects with no
/// `dlc_pin_data`.
#[tokio::test]
#[serial]
async fn j1708_ch1_establishes_independently_alongside_a_j1708_ps_sibling() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let ps_cll =
        create_and_connect_cll_for_module(&mut client, MOCK_MODULE_HANDLE, J1708_RESOURCE_ID, &[])
            .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_J1708_PS
    );

    let chx_cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_J1708_CH1, &[]), // _CH1
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
            "connect_com_logical_link should succeed for a directly-named PROTOCOL_J1708_CH1 id",
        );

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the _PS sibling and its own _CH1 Additional Channel should open two distinct physical \
         channels"
    );
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID + 1),
        j2534_0404::PROTOCOL_J1708_CH1,
        "the second connect should open a native PROTOCOL_J1708_CH1 channel"
    );

    let _ = ps_cll;
    server.shutdown().await;
}

/// Regression test: before the mechanical extension of ADR-211's/ADR-212's
/// established pattern (`resources.rs::chx_device_info_supported_
/// parameter` gaining a SAE J1708 guard arm), this function returned
/// `None` for every SAE J1708 id, so `check_chx_capacity`'s
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
/// it. Simplified relative to that FT-CAN test since SAE J1708 shares the
/// generic `chx_capacity` override (no per-family override is needed --
/// this family is independently self-identifying, unlike FT-CAN/SW-CAN,
/// which collapse onto the generic CAN/ISO15765 base via
/// `base_protocol_id`).
#[tokio::test]
#[serial]
async fn connect_rejects_a_chx_index_above_the_cached_capacity() {
    let server = start_j2534_2_server().await;
    server.backdoor.set_chx_capacity(1);
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_J1708_CH1 + 1, &[]), // _CH2
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
        .expect_err("_CH2 exceeds the cached capacity (1) -- the connect must be rejected");
    assert_eq!(
        status.code(),
        Code::InvalidArgument,
        "must be rejected by check_chx_capacity's Discovery precheck (Code::InvalidArgument), \
         not fall through to this mock's own independent native-level _CHx capacity check (a \
         different, non-InvalidArgument code) -- if this fires, chx_device_info_supported_\
         parameter returned None for SAE J1708 (the bug this fix corrects)"
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

/// Mirrors `honda_diagh.rs`'s own `honda_diagh_connecting_via_compound_chx_name_succeeds`
/// (ADR-208)/`uart_echo_byte.rs`'s `connecting_via_compound_chx_name_succeeds`
/// (ADR-207)/`gm_uart.rs`'s/`j1939.rs`'s own equivalents (ADR-206):
/// connecting via the compound `_CHx`-suffixed `protocol_name` grammar
/// ("SAE_J1708_CH1", the resource-table row's own `protocol_name` "SAE_J1708"
/// with a `_CH1` suffix) succeeds the same way the raw-id route above does --
/// this is the test that specifically proves the `names.rs`
/// `requested_index.is_some()` bypass fix (ADR-209 Decision item 5) works:
/// without it, this connect would fail with the clause-6/clause-7
/// mutual-exclusion rejection instead.
#[tokio::test]
#[serial]
async fn j1708_connecting_via_compound_chx_name_succeeds() {
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
                        "SAE_J1708_CH1".to_string(),
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
        j2534_0404::PROTOCOL_J1708_CH1,
        "the compound-name route should open a native PROTOCOL_J1708_CH1 channel, same as the \
         raw protocol-id route"
    );

    server.shutdown().await;
}

/// Regression test for ADR-209 Decision item 8: mirrors
/// `start_repeat_message_applies_the_configured_message_priority` above (the
/// `_PS`-connected proof that `rpc_misc.rs::ioctl_start_repeat_message`'s
/// TxFlags composition correctly ORs in `MSG_PRIORITY_VALUE`), but connects
/// via a directly-named `PROTOCOL_J1708_CH1` id instead of the `_PS`
/// resource id -- before the fix, that composition site was gated on the
/// narrower, arm-gate-only `is_j1708_protocol_id` (an exact `_PS`-only
/// match), so a `_CHx`-connected link's `CP_MessagePriority` value would
/// never have reached `MSG_PRIORITY_VALUE` on a repeat-messaging
/// transmission, even though clause 17 places no Repeat Messaging exclusion
/// on this protocol at all.
#[tokio::test]
#[serial]
async fn j1708_chx_start_repeat_message_applies_the_configured_message_priority() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_J1708_CH1,
        &[],
        &[(CP_MESSAGE_PRIORITY, 3)],
    )
    .await;

    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_J1708_CH1,
        "this test must actually exercise a _CHx-connected link, not silently fall back to _PS"
    );

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let _msg_id = start_repeat_message_unconditional(
        &mut client,
        cll_handle,
        start_id,
        vec![0x01, 0x02, 0x03],
    )
    .await;

    wait_for_written_count(&server, MOCK_CHANNEL_ID, 1).await;
    let tx_flags = server.backdoor.written_tx_flags(MOCK_CHANNEL_ID, 0);
    assert_eq!(
        tx_flags & j2534_0404::TX_FLAG_MSG_PRIORITY_VALUE,
        3 << 16,
        "an in-range CP_MessagePriority (3) should reach TxFlags bits 16-19 on the actually- \
         transmitted RepeatMsgData[0] message for a _CHx-connected J1708 link, the same as its \
         _PS sibling; got {tx_flags:#010x}"
    );

    server.shutdown().await;
}

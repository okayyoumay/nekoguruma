//! End-to-end coverage for SAE J2534-2 clause 11 GM UART Protocol (SAE
//! J2740, ADR-189, Phase 8): connecting the new standalone `GM_UART_PS`
//! resource row's mandatory internal `SET_CONFIG(CONFIG_J1962_PINS)`, the
//! row's own pin-9 default, the closed two-single-pin set (clause 11.2.2:
//! pin 1 or pin 9, and no other), the two new thin `ChannelID`-scoped
//! `PDU_IOCTL_SET_POLL_RESPONSE`/`PDU_IOCTL_BECOME_MASTER` IOCTLs, the
//! ADR-185 Stage 1 Discovery-cache connect-path gate, and the clause 7
//! Additional Channels (`_CHx`) arithmetic mapping this protocol is the
//! first standalone family to combine with a single resource-table row
//! (ADR-189 Decision 2). Mirrors `honda_diagh.rs`'s structure/helpers for
//! the connect/pin-selection coverage (the closest existing closed-two-
//! single-pin precedent), `sw_can.rs`'s structure for the two new IOCTLs'
//! own coverage (the closest existing `ChannelID`-scoped protocol-gated
//! IOCTL precedent), and `additional_channels.rs`'s structure for the `_CHx`
//! coverage; a few small per-file-local helpers are duplicated the same way
//! every sibling file in this directory duplicates its own (this codebase's
//! existing convention for this shape of helper, not shared via
//! `harness.rs`).
//!
//! Unlike every standalone protocol before it (UART Echo Byte/Honda
//! DIAG-H/J1708/J1939/TP2.0), `ChannelProtocol::GM_UART_PS` also
//! participates in the clause 7 Additional Channels `_CHx` arithmetic
//! mapping (ADR-189 Decision 2) -- Item (d) below is this protocol's own
//! addition to the existing `_PS`-only test shape every sibling file uses.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComLogicalLinkHandle, ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, DataItem,
    IoBytearray, ModuleHandle, PduError, PinData, ResourceData, create_com_logical_link_request,
    data_item, error_detail_from_status, io_ctl_request, resource_data,
    vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// Resource id of the SAE J2534-2 clause 11 GM UART Protocol row --
/// `resources.rs` row 0x0260, `protocol: ChannelProtocol::GM_UART_PS`,
/// `hw_protocol_override: None` (unlike SWCAN/FT-CAN, there is no separate
/// base id to override onto -- the `_PS` id IS the identity).
const GM_UART_RESOURCE_ID: u32 = 0x0260;

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// every GM UART resource row requires this opt-in (`names.rs`'s dedicated
/// arm in `resolve_pin_selection`, mirroring the Honda DIAG-H/UART Echo
/// Byte opt-in gates).
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

/// Builds a `RscData` resource directly naming `raw_protocol_id` via the
/// unambiguous `ProtocolId` route, no pins -- same shape as
/// `additional_channels.rs`'s own `resource_with_protocol_id`, for the
/// `_CHx` coverage (Item (d)).
fn resource_with_protocol_id(raw_protocol_id: u32) -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![],
        bus_type: None,
        protocol: Some(resource_data::Protocol::ProtocolId(raw_protocol_id)),
    }
}

/// Like [`create_and_connect_cll_for_module`] (harness.rs), but for the raw
/// protocol-id + explicit-pins route -- same shape as `honda_diagh.rs`'s own
/// `create_and_connect_cll_for_protocol_id_and_pins`.
async fn create_and_connect_cll_for_protocol_id_and_pins(
    client: &mut VciServiceClient<tonic::transport::Channel>,
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

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id`
/// via `GetObjectId(OBJT_IO_CTRL, ...)` -- same per-file local helper shape
/// as `sw_can.rs`'s/`honda_diagh.rs`'s own.
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

/// Issues `IoCtl` against a `cll_handle` with an optional `input_data`,
/// no output expected -- same shape as `honda_diagh.rs`'s own `io_ctl_cll`,
/// narrowed to the no-output case both new GM UART IOCTLs use.
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
            has_output: false,
        })
        .await
        .map(|_| ())
}

fn bytearray_input(data: Vec<u8>) -> DataItem {
    DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray { data })),
    }
}

fn unum32_input(value: u32) -> DataItem {
    DataItem {
        data: Some(data_item::Data::Unum32Value(value)),
    }
}

// ── Item (a): connect + pin selection ───────────────────────────────────────

/// Connecting via the resource id succeeds, resolves and connects with
/// `PROTOCOL_GM_UART_PS` as the hardware protocol id, and the connect
/// sequence includes the mandatory internal `SET_CONFIG(CONFIG_J1962_PINS)`
/// -- clause 11.2.2's explicit-pin-only model (ADR-189 Decision
/// 3). With no caller-supplied `dlc_pin_data`, the row's own pin-9 default
/// is what gets assigned, packed `0x0000_0900` (SS = 0, no secondary pin).
#[tokio::test]
#[serial]
async fn connecting_via_resource_id_applies_the_default_pin() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        GM_UART_RESOURCE_ID,
        &[],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_GM_UART_PS,
        "connecting resource 0x0260 should open a native GM_UART_PS channel"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0900,
        "an unqualified GM UART connect (no dlc_pin_data) should assign the row's own default \
         pin (9), packed 0x0000PPSS"
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "CONFIG_J1962_PINS should have been SET_CONFIG'd during connect; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Connecting the raw `GM_UART_PS` id directly with explicit `dlc_pin_data`
/// selecting pin 1 succeeds -- clause 11.2.2's second documented pin.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_with_pin_1_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_GM_UART_PS,
        &[(1, "K")],
        &[],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_GM_UART_PS
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0100,
        "an explicit pin 1 request should be accepted and assigned verbatim"
    );

    server.shutdown().await;
}

/// Connecting the raw `GM_UART_PS` id directly with explicit `dlc_pin_data`
/// selecting pin 9 also succeeds -- clause 11.2.2's primary pin, the same
/// pin the resource-table row's own default (above) uses.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_with_pin_9_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_GM_UART_PS,
        &[(9, "K")],
        &[],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0900,
        "an explicit pin 9 request should be accepted and assigned verbatim"
    );

    server.shutdown().await;
}

/// Naming the raw `GM_UART_PS` hardware protocol id directly (bypassing the
/// resource table entirely) with no `dlc_pin_data` must be rejected, not
/// silently connected with the resource-table row's own default pin --
/// clause 11.2.2 itself identifies no default pin at all.
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
                        j2534_0404::PROTOCOL_GM_UART_PS,
                    )),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "naming the raw GM_UART_PS id directly with no dlc_pin_data must be rejected, not \
             silently defaulted to the resource-table row's own pin",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("no default pin"),
        "the rejection should explain that clause 11.2.2 has no default pin: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Connecting the raw `GM_UART_PS` id directly with explicit `dlc_pin_data`
/// selecting any OTHER single pin (pin 6, well-formed but not one of clause
/// 11.2.2's two documented pins) is rejected -- the two-value closed-set
/// check, not an open choice.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_with_an_undocumented_pin_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_GM_UART_PS, &[(6, "K")]),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "selecting pin 6 (well-formed, but not one of clause 11.2.2's two documented pins) \
             must be rejected",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("pin 1 or pin 9"),
        "the rejection should explain clause 11.2.2's two documented pins: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejected connect must never reach the native PassThruConnect call"
    );

    server.shutdown().await;
}

/// Supplying a SECOND (secondary) pin alongside a valid primary must be
/// rejected, not silently packed via the general clause-6 two-pin path --
/// this protocol is single-wire, with no secondary pin role at all.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_with_two_pins_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(
                    j2534_0404::PROTOCOL_GM_UART_PS,
                    &[(1, "K"), (9, "K")],
                ),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "supplying a secondary pin for the single-wire GM_UART_PS bus must be rejected, not \
             silently packed as a two-pin CONFIG_J1962_PINS value",
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

// ── Name-based protocol route (Codex PR #98 regression): GM UART named route

/// Regression test for Codex finding PR #98: connecting via the canonical
/// `protocol_name` ("GM_UART", row 0x0260's own name) with no explicit
/// `dlc_pin_data` must correctly resolve through `resolve_pin_selection`
/// (requiring J2534-2 opt-in), which issues the mandatory `CONFIG_J1962_PINS`
/// with the row's own pin-9 default. Before the fix, `row_needs_dynamic_pin_selection`
/// never registered `resources::is_gm_uart_protocol_id`, so the name-based
/// connect bypassed `resolve_pin_selection` entirely and took the
/// "fixed-row pin narrowing" path, never issuing `CONFIG_J1962_PINS` and
/// incorrectly treating the row's default as unavailable.
#[tokio::test]
#[serial]
async fn connecting_via_canonical_name_without_pins_applies_the_default_pin() {
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
                    protocol: Some(resource_data::Protocol::ProtocolName("GM_UART".to_string())),
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
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0900,
        "the canonical-name route with no dlc_pin_data must assign the resource-table row's own \
         default pin (9), not reject it or fail to issue CONFIG_J1962_PINS"
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "CONFIG_J1962_PINS should have been SET_CONFIG'd during the name-based connect; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Regression test for Codex finding PR #98: connecting via the canonical
/// `protocol_name` ("GM_UART") with explicit `dlc_pin_data` selecting pin 1
/// must succeed -- clause 11.2.2 documents pin 1 as a valid connection pin,
/// and `resolve_pin_selection`'s own closed-set check already accepts it on
/// the raw-id route. Before the fix, `find_table_row_by_name` narrowed the
/// single matching row against its own resource-table pin-9 convenience default
/// *before* `resolve_pin_selection` ever ran (because `row_needs_dynamic_pin_selection`
/// didn't recognize GM UART), rejecting the legitimate pin 1 choice.
#[tokio::test]
#[serial]
async fn connecting_via_canonical_name_with_pin_1_succeeds() {
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
                                "K".to_string(),
                            ),
                        ),
                    }],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolName("GM_UART".to_string())),
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
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0100,
        "the canonical-name route must assign the caller's own explicit pin 1, not reject it \
         against the resource-table row's pin-9 convenience default"
    );

    server.shutdown().await;
}

// ── Items (b)/(c): the two new IOCTLs ───────────────────────────────────────

/// Item (b): `PDU_IOCTL_SET_POLL_RESPONSE` round-trips bytes into the mock's
/// per-channel state on a connected GM UART link, and is rejected
/// (`PDU_ERR_ID_NOT_SUPPORTED`) on a non-GM-UART link.
#[tokio::test]
#[serial]
async fn set_poll_response_round_trips_and_is_rejected_on_a_non_gm_uart_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let gm_uart_cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        GM_UART_RESOURCE_ID,
        &[],
    )
    .await;
    let can_cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let set_poll_response_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_POLL_RESPONSE").await;

    let poll_response = vec![0x11, 0x22, 0x33, 0x44];
    io_ctl_cll(
        &mut client,
        gm_uart_cll,
        set_poll_response_id,
        Some(bytearray_input(poll_response.clone())),
    )
    .await
    .expect("PDU_IOCTL_SET_POLL_RESPONSE should succeed on a connected GM UART link");
    assert_eq!(
        server.backdoor.poll_response(MOCK_CHANNEL_ID),
        poll_response,
        "the staged poll-response bytes should reach the native adapter verbatim"
    );

    let status = io_ctl_cll(
        &mut client,
        can_cll,
        set_poll_response_id,
        Some(bytearray_input(vec![0xAA])),
    )
    .await
    .expect_err("PDU_IOCTL_SET_POLL_RESPONSE should be rejected on a non-GM-UART CLL");
    assert_eq!(status.code(), Code::Unimplemented);
    assert!(status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"));

    server.shutdown().await;
}

/// Regression test for Codex finding PR #98 round 5: SAE J2534-2 clause
/// 11.3.3.1's `PollResponseMsg[100]` is a fixed 100-byte array (Table 28).
/// The service intentionally delegates this bound to the native layer
/// (ADR-189 Decision item 4 -- a thin passthrough, no client-side length
/// pre-validation), so `j2534-0404-mock`'s own `IOCTL_SET_POLL_RESPONSE`
/// handler is what stands in for a conforming adapter's rejection here.
#[tokio::test]
#[serial]
async fn set_poll_response_rejects_a_payload_over_100_bytes() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let gm_uart_cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        GM_UART_RESOURCE_ID,
        &[],
    )
    .await;

    let set_poll_response_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_POLL_RESPONSE").await;

    let oversized = vec![0xAB; 101];
    let status = io_ctl_cll(
        &mut client,
        gm_uart_cll,
        set_poll_response_id,
        Some(bytearray_input(oversized)),
    )
    .await
    .expect_err(
        "PDU_IOCTL_SET_POLL_RESPONSE with more than 100 bytes should be rejected, not silently \
         accepted and truncated/forwarded",
    );
    assert_eq!(status.code(), Code::Internal);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrValueNotSupported as i32
    );

    server.shutdown().await;
}

/// Item (c): `PDU_IOCTL_BECOME_MASTER` succeeds by default on a connected GM
/// UART link, surfaces a mock-injected native failure via
/// `__mock_set_become_master_error`, and is rejected
/// (`PDU_ERR_ID_NOT_SUPPORTED`) on a non-GM-UART link.
#[tokio::test]
#[serial]
async fn become_master_succeeds_surfaces_injected_failure_and_is_rejected_on_a_non_gm_uart_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let gm_uart_cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        GM_UART_RESOURCE_ID,
        &[],
    )
    .await;
    let can_cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let become_master_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_BECOME_MASTER").await;

    io_ctl_cll(
        &mut client,
        gm_uart_cll,
        become_master_id,
        Some(unum32_input(0)),
    )
    .await
    .expect(
        "PDU_IOCTL_BECOME_MASTER should succeed by default (Poll_ID 0: immediate grant, clause \
         11.3.3.2) on a connected GM UART link",
    );

    server
        .backdoor
        .set_become_master_error(Some(j2534_0404::ERR_FAILED as std::os::raw::c_long));
    let failed = io_ctl_cll(
        &mut client,
        gm_uart_cll,
        become_master_id,
        Some(unum32_input(5)),
    )
    .await
    .expect_err(
        "PDU_IOCTL_BECOME_MASTER should surface a mock-injected ERR_FAILED (\"no poll message \
         within 2s\", clause 11.3.3.2) as a native failure",
    );
    assert_eq!(failed.code(), Code::Internal);
    assert!(
        failed.message().contains("PDU_ERR_FCT_FAILED") || failed.message().contains("ERR_FAILED"),
        "unexpected error message: {}",
        failed.message()
    );
    server.backdoor.set_become_master_error(None);

    let status = io_ctl_cll(
        &mut client,
        can_cll,
        become_master_id,
        Some(unum32_input(0)),
    )
    .await
    .expect_err("PDU_IOCTL_BECOME_MASTER should be rejected on a non-GM-UART CLL");
    assert_eq!(status.code(), Code::Unimplemented);
    assert!(status.message().contains("PDU_ERR_ID_NOT_SUPPORTED"));

    server.shutdown().await;
}

// ── PR #98 regression: shared-channel/in-flight rejections ─────────────────

/// Codex review P2 finding (PR #98): `ioctl_become_master`'s `ref_count ==
/// 1` sole-owner gate used to be checked, `shared_channels` released, and
/// only then did the ~2s blocking native call run -- leaving a window where
/// a sibling CLL's `ConnectComLogicalLink` could join the SAME physical
/// channel while the mastership bid was still outstanding (the join path
/// bumps `ref_count` under its own, separate `shared_channels` acquisition
/// and never touches `self.api`, so nothing serialized it against the
/// in-flight call), defeating the sole-owner precaution entirely. Proves
/// the fix (`SharedChannel::become_master_in_flight`) actually closes the
/// window: a sibling connect attempted while the mock's
/// `IOCTL_BECOME_MASTER` call is held open (`MockBackdoor::
/// arm_become_master_hold`) is rejected with
/// `PDU_ERR_RSC_LOCKED_BY_OTHER_CLL`/`ResourceExhausted`; once the held call
/// is released and completes, a THIRD connect attempt on the same resource
/// succeeds normally -- proving the flag clears afterward and does not
/// leak.
#[tokio::test]
#[serial]
async fn become_master_in_flight_rejects_a_concurrent_sibling_connect_then_clears() {
    // `edge-case-hunter` finding (PR #98): without this guard, a failed
    // assertion between arming and releasing the hold below would leave the
    // mock's `IOCTL_BECOME_MASTER` native-call thread blocked forever --
    // repro-confirmed to hang the whole test binary at Tokio runtime
    // teardown (waiting on the orphaned `spawn_blocking` task) rather than
    // failing cleanly with the assertion's own message. `Drop` releases the
    // hold unconditionally on every exit path (normal return, `?`, or a
    // panicking assertion via unwind), so a real regression here fails fast
    // with a clear message instead of reading as an infra hang.
    struct ReleaseOnDrop<'a>(&'a TestServer);
    impl Drop for ReleaseOnDrop<'_> {
        fn drop(&mut self) {
            self.0.backdoor.release_become_master_hold();
        }
    }

    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        GM_UART_RESOURCE_ID,
        &[],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);

    let become_master_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_BECOME_MASTER").await;

    server.backdoor.arm_become_master_hold();
    let release_guard = ReleaseOnDrop(&server);

    let mut become_master_client = client.clone();
    let become_master_task = tokio::spawn(async move {
        io_ctl_cll(
            &mut become_master_client,
            cll_a,
            become_master_id,
            Some(unum32_input(0)),
        )
        .await
    });

    wait_for_become_master_hold_engaged(&server).await;

    // A sibling CLL's connect attempt, landing while the BECOME_MASTER bid
    // above is still held open, must be rejected outright -- not silently
    // joined onto the same physical channel.
    let cll_b =
        create_cll_for_module(&mut client, MOCK_MODULE_HANDLE, GM_UART_RESOURCE_ID, 2).await;
    let joined_while_in_flight = client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_b),
        })
        .await
        .expect_err(
            "a sibling connect attempted while BECOME_MASTER is in flight must be rejected, \
             not silently joined onto the same physical channel",
        );
    assert_eq!(joined_while_in_flight.code(), Code::ResourceExhausted);
    assert!(
        joined_while_in_flight
            .message()
            .contains("PDU_ERR_RSC_LOCKED_BY_OTHER_CLL"),
        "unexpected rejection message: {}",
        joined_while_in_flight.message()
    );

    server.backdoor.release_become_master_hold();
    // Explicit release above already covers the normal-exit path; drop the
    // guard now (its own release is a documented no-op once already
    // released) so `server` is no longer borrowed before `server.shutdown()`
    // moves it below.
    drop(release_guard);
    become_master_task
        .await
        .expect("become_master task should not panic")
        .expect("PDU_IOCTL_BECOME_MASTER should succeed once released");

    // The flag must have cleared once the call completed: a THIRD connect
    // attempt, now that the bid is done, succeeds normally and joins the
    // existing channel rather than opening a new one.
    let cll_c = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        GM_UART_RESOURCE_ID,
        &[],
    )
    .await;
    let _ = cll_c;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "cll_c should have JOINED the existing physical channel (proving \
         become_master_in_flight cleared, not leaked), not opened a new one"
    );

    server.shutdown().await;
}

/// Codex review finding (PR #98): a GM UART physical channel already shared
/// with a sibling CLL (`SharedChannel::ref_count != 1`) used to make both
/// `PDU_IOCTL_BECOME_MASTER` and `PDU_IOCTL_SET_POLL_RESPONSE` silently
/// no-op (`Ok(())`, no native call at all) instead of rejecting -- a lie,
/// since the caller sees the exact same success response an IOCTL that
/// actually ran would return. Both now reject with
/// `PDU_ERR_RSC_LOCKED_BY_OTHER_CLL`/`ResourceExhausted`, the same shape a
/// sibling CLL's own join attempt gets.
#[tokio::test]
#[serial]
async fn become_master_and_set_poll_response_reject_a_shared_physical_channel() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_a = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        GM_UART_RESOURCE_ID,
        &[],
    )
    .await;
    let cll_b = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        GM_UART_RESOURCE_ID,
        &[],
    )
    .await;
    assert_eq!(
        server.backdoor.connect_count(),
        1,
        "sanity: cll_a and cll_b must actually share one physical channel for this test to \
         exercise the ref_count != 1 gate at all"
    );

    let become_master_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_BECOME_MASTER").await;
    let become_master_status =
        io_ctl_cll(&mut client, cll_a, become_master_id, Some(unum32_input(0)))
            .await
            .expect_err(
                "PDU_IOCTL_BECOME_MASTER on a shared channel must be rejected, not silently no-op",
            );
    assert_eq!(become_master_status.code(), Code::ResourceExhausted);
    assert!(
        become_master_status
            .message()
            .contains("PDU_ERR_RSC_LOCKED_BY_OTHER_CLL"),
        "unexpected rejection message: {}",
        become_master_status.message()
    );

    let set_poll_response_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_SET_POLL_RESPONSE").await;
    let set_poll_response_status = io_ctl_cll(
        &mut client,
        cll_a,
        set_poll_response_id,
        Some(bytearray_input(vec![0xAA])),
    )
    .await
    .expect_err(
        "PDU_IOCTL_SET_POLL_RESPONSE on a shared channel must be rejected, not silently no-op",
    );
    assert_eq!(set_poll_response_status.code(), Code::ResourceExhausted);
    assert!(
        set_poll_response_status
            .message()
            .contains("PDU_ERR_RSC_LOCKED_BY_OTHER_CLL"),
        "unexpected rejection message: {}",
        set_poll_response_status.message()
    );

    let _ = cll_b;
    server.shutdown().await;
}

// ── Item (d): clause 7 Additional Channels (_CHx) ───────────────────────────

/// A directly-named `GM_UART_CH1` id establishes its own physical channel
/// independently of a `GM_UART_PS` sibling connected on the same module --
/// mirrors `additional_channels.rs`'s `two_chx_links_at_different_indices_
/// get_distinct_channels`, but pairing the `_PS` resource-table row with its
/// own `_CHx` sibling rather than two `_CHx` indices, the combination unique
/// to GM UART among the standalone protocols this service implements
/// (ADR-189 Decision 2).
#[tokio::test]
#[serial]
async fn gm_uart_ch1_establishes_independently_alongside_a_gm_uart_ps_sibling() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let ps_cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        GM_UART_RESOURCE_ID,
        &[],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_GM_UART_PS
    );

    let chx_cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id(j2534_0404::PROTOCOL_GM_UART_CH1), // _CH1
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
        .expect("connect_com_logical_link should succeed for a directly-named GM_UART_CH1 id");

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the _PS sibling and its own _CH1 Additional Channel should open two distinct physical \
         channels"
    );
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID + 1),
        j2534_0404::PROTOCOL_GM_UART_CH1,
        "the second connect should open a native GM_UART_CH1 channel"
    );

    let _ = ps_cll;
    server.shutdown().await;
}

/// Regression test for Codex finding PR #98 round 3: connecting via the
/// compound `_CHx`-suffixed `protocol_name` grammar ("GM_UART_CH1") must
/// succeed like the raw `PROTOCOL_GM_UART_CHx` route above -- GM UART is the
/// first standalone-row protocol whose canonical name is ALSO recognized by
/// `row_needs_dynamic_pin_selection` (needed so a plain "GM_UART" connect
/// gets its mandatory pin selection, the P1 fix earlier in this file), which
/// before this fix meant a compound name's head resolution still forced
/// `resolve_pin_selection` to compute the row's own default pin selection
/// even when `requested_index` (parsed from the "_CH1" suffix) was set --
/// `resolve_channel_selection` then rejected the request as combining Pin
/// Selection with a clause 7 Additional Channel, even though `_CHx` channels
/// have no J1962 pin concept at all.
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
                        "GM_UART_CH1".to_string(),
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
        j2534_0404::PROTOCOL_GM_UART_CH1,
        "the compound-name route should open a native GM_UART_CH1 channel, same as the raw \
         protocol-id route"
    );

    server.shutdown().await;
}

// ── Item (e): ADR-185 Stage 1 Discovery-cache connect-path gate ────────────

/// Connecting a GM UART resource fails with the Discovery-driven rejection
/// when the mock reports `DEVICE_INFO_GM_UART_SUPPORTED` as unsupported --
/// mirrors every other ADR-185 Stage 1 family's own connect-time fail-fast
/// coverage (ADR-189 Decision 5).
#[tokio::test]
#[serial]
async fn connect_fails_when_discovery_reports_gm_uart_unsupported() {
    let server = start_j2534_2_server().await;
    server.backdoor.set_gm_uart_supported(false);
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_GM_UART_PS, &[(9, "K")]),
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
             when the device does not advertise DEVICE_INFO_GM_UART_SUPPORTED",
        );
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the Discovery precheck must reject before any native PassThruConnect is attempted"
    );

    server.shutdown().await;
}

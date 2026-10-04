//! End-to-end coverage for SAE J2534-2 clause 13 Honda DIAG-H Protocol
//! (ADR-174, Phase 10): connecting the new standalone `HONDA_DIAGH_PS`
//! resource row's mandatory internal `SET_CONFIG(CONFIG_J1962_PINS)` (clause
//! 13.2.3's explicit-pin-only model), the row's own pin-14
//! default, the closed two-single-pin set (clause 13.2.4: pin 1 or pin 14,
//! and no other), the closed ComParam allowlist (clause 13.3.3.1, notably
//! excluding `DATA_RATE`), and the `is_kline == false` classification
//! (ADR-174 Decision 5) -- a non-empty `CoptStartcomm cop_data` is a genuine
//! optional message, not a 5-baud/fast-init address. Mirrors
//! `uart_echo_byte.rs`'s structure for the connect/pin/ComParam coverage,
//! and `startcomm_optional_message_tx.rs`'s CAN/J1850 optional-message
//! pattern for the `is_kline == false` coverage; a few small per-file-local
//! helpers are duplicated the same way `uart_echo_byte.rs` duplicates its
//! own (this codebase's existing convention for this shape of helper, not
//! shared via `harness.rs`).
//!
//! Unlike SWCAN/FT-CAN, this protocol has no CAN/ISO15765-family
//! relationship at all -- `ChannelProtocol::HONDA_DIAGH_PS` is a wholly new,
//! standalone identity (ADR-174 Context/Decision 1), the same shape as UART
//! Echo Byte (ADR-170). Unlike UART Echo Byte, though, this protocol closes
//! its pin set to TWO single pins (not one), excludes `DATA_RATE` from its
//! ComParam allowlist (fixed 9600bps, clause 13.3.1), and is `is_kline ==
//! false` (no init sequence at all, clause 13.3.1) -- these three
//! differences are what this file's own tests are built around.

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveCtrlData, ConnectComLogicalLinkRequest,
    CreateComLogicalLinkRequest, DataItem, ExpectedResponseData, GetComParamRequest, IoBytearray,
    ModuleHandle, ParamItem, PduError, PduParamClass, PinData, ResourceData, SetComParamRequest,
    StartComPrimitiveRequest, create_com_logical_link_request, data_item, error_detail_from_status,
    event_item, get_com_param_request, io_ctl_request, param_item, resource_data,
    subscribe_event_request, vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// Resource id of the SAE J2534-2 clause 13 Honda DIAG-H Protocol row --
/// `resources.rs` row 0x023B, `protocol: ChannelProtocol::HONDA_DIAGH_PS`,
/// `hw_protocol_override: None` (unlike SWCAN/FT-CAN, there is no separate
/// base id to override onto -- the `_PS` id IS the identity).
const HONDA_DIAGH_RESOURCE_ID: u32 = 0x023B;

/// `CP_InitializationSettings`'s native `ComParamId` value (0x8090,
/// `service_params::PARAM_INIT_SETTINGS`) -- not re-exported from `j2534_0404`,
/// so named directly here the same way `uart_echo_byte.rs`'s sibling file
/// names `CP_TESTER_PRESENT_SEND_TYPE`.
const CP_INITIALIZATION_SETTINGS: u32 = 0x8090;

/// A module opted into SAE J2534-2 (`"J2534-2:"` `pname` prefix, clause 5) --
/// every Honda DIAG-H resource row requires this opt-in (`names.rs`'s
/// dedicated arm in `resolve_pin_selection`, mirroring the SWCAN/FTCAN/UART
/// Echo Byte opt-in gates).
async fn start_j2534_2_server() -> TestServer {
    TestServer::start_with_modules(&[("Bench 1", "J2534-2:mock")]).await
}

/// Builds a `RscData` resource selecting `protocol_id` via the raw
/// hardware-protocol-id route, with the given typed `(pin_number,
/// pin_type_name)` pairs as `dlc_pin_data` -- same shape as
/// `uart_echo_byte.rs`'s own `resource_with_protocol_id_and_pins`.
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
/// protocol-id + explicit-pins route -- same shape as `uart_echo_byte.rs`'s
/// own `create_and_connect_cll_for_protocol_id_and_pins`.
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

/// Item 1: connecting via the resource id succeeds, resolves and connects
/// with `PROTOCOL_HONDA_DIAGH_PS` as the hardware protocol id, and the
/// connect sequence includes the mandatory internal
/// `SET_CONFIG(CONFIG_J1962_PINS)` -- clause 13.2.3's explicit-pin-only
/// model (ADR-174 Decision 2). With no caller-supplied
/// `dlc_pin_data`, the row's own default pin (14) is what gets assigned,
/// packed `0x0000_0E00` (SS = 0, no secondary pin).
#[tokio::test]
#[serial]
async fn connecting_via_resource_id_applies_the_default_pin() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        HONDA_DIAGH_RESOURCE_ID,
        &[],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_HONDA_DIAGH_PS,
        "connecting resource 0x023B should open a native HONDA_DIAGH_PS channel"
    );
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0E00,
        "an unqualified Honda DIAG-H connect (no dlc_pin_data) should assign the row's own \
         default pin (14), packed 0x0000PPSS"
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&j2534_0404::CONFIG_J1962_PINS),
        "CONFIG_J1962_PINS should have been SET_CONFIG'd during connect; log was {log:#x?}"
    );

    server.shutdown().await;
}

/// Regression test (Codex review, PR #63 round 4): connecting via the
/// canonical `protocol_name` ("HONDA_DIAGH", row 0x023B's own name) with
/// explicit `dlc_pin_data` selecting pin 1 must succeed -- clause 13.2.4
/// documents pin 1 as a valid connection pin, and `resolve_pin_selection`'s
/// own closed-set check (Item 3 below) already accepts it on the raw-id
/// route. Before the fix, `find_table_row_by_name` narrowed the single
/// matching row against its own resource-table pin-14 convenience default
/// *before* `resolve_pin_selection` ever ran, rejecting a legitimate pin
/// choice with a generic "matches no configuration" error.
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
                    protocol: Some(resource_data::Protocol::ProtocolName(
                        "HONDA_DIAGH".to_string(),
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
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0100,
        "the canonical-name route must assign the caller's own explicit pin 1, not reject it \
         against the resource-table row's pin-14 convenience default"
    );

    server.shutdown().await;
}

/// Item 2: naming the raw `HONDA_DIAGH_PS` hardware protocol id directly
/// (bypassing the resource table entirely) with no `dlc_pin_data` must be
/// rejected, not silently connected with the resource-table row's own
/// default pin -- clause 13.2.3 itself identifies no default pin at all.
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
                        j2534_0404::PROTOCOL_HONDA_DIAGH_PS,
                    )),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "naming the raw HONDA_DIAGH_PS id directly with no dlc_pin_data must be rejected, \
             not silently defaulted to the resource-table row's own pin",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("no default pin"),
        "the rejection should explain that clause 13.2.3 has no default pin: {}",
        status.message()
    );

    server.shutdown().await;
}

/// Item 3: connecting the raw `HONDA_DIAGH_PS` id directly with explicit
/// `dlc_pin_data` selecting pin 1 succeeds -- clause 13.2.4's first
/// documented pin.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_with_pin_1_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_HONDA_DIAGH_PS,
        &[(1, "K")],
        &[],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_HONDA_DIAGH_PS
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

/// Item 4 (sibling of Item 3): connecting the raw `HONDA_DIAGH_PS` id
/// directly with explicit `dlc_pin_data` selecting pin 14 also succeeds --
/// clause 13.2.4's second documented pin, and the same pin the resource-table
/// row's own default (Item 1) uses.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_with_pin_14_succeeds() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_HONDA_DIAGH_PS,
        &[(14, "K")],
        &[],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::CONFIG_J1962_PINS),
        0x0000_0E00,
        "an explicit pin 14 request should be accepted and assigned verbatim"
    );

    server.shutdown().await;
}

/// Regression test (Codex review, PR #63 round 2): connecting via the raw
/// `HONDA_DIAGH_PS` id bypasses the resource table entirely (`resource_row ==
/// None`, `bus_type: None` in `resource_with_protocol_id_and_pins`), so
/// `rpc_create_com_logical_link` cannot select `honda_diagh_uart()` via its
/// usual `resource_row`/`bus_type_name` lookup -- without the round-2 fix,
/// the Working set stays fully empty and `ComParamSet::baud_rate()` feeds
/// `PassThruConnect` a native baud rate of 0 instead of clause 13.3.1's
/// mandatory 9600bps. Asserts the native connect actually receives 9600,
/// the same value the resource-id route (Item 1, which already goes through
/// `resource_row.is_some()`) gets for free.
#[tokio::test]
#[serial]
async fn connecting_the_raw_protocol_id_seeds_the_mandatory_9600_baud_rate() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let _cll = create_and_connect_cll_for_protocol_id_and_pins(
        &mut client,
        j2534_0404::PROTOCOL_HONDA_DIAGH_PS,
        &[(1, "K")],
        &[],
    )
    .await;

    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.baud_rate(MOCK_CHANNEL_ID),
        9_600,
        "the raw-id no-resource-row connect route must still receive clause \
         13.3.1's mandatory 9600bps, not the empty Working set's default of 0"
    );
    // edge-case-hunter finding (PR #63 close-out sweep): round 2's
    // comparam-defaulting fix and round 3's to_j2534_config_id translation
    // fix were each tested against a different connect route (resource-id
    // vs. raw-id) -- nothing regression-tested them jointly on the raw-id
    // route both were written for. P1_MAX = 20_000us -> 40 (0.5ms-resolution
    // units, ADR-072), same as the resource-id route's own
    // `p1_max_p3_min_p4_min_reach_the_native_adapter_via_set_config`.
    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P1_MAX),
        40,
        "the raw-id no-resource-row connect route must also forward the seeded P1_MAX default \
         to the native adapter, not just DATA_RATE"
    );

    server.shutdown().await;
}

/// Regression test (edge-case-hunter finding, PR #63 round 2): a caller
/// naming `HONDA_DIAGH_PS` via `protocol` while ALSO supplying an unrelated,
/// well-formed `bus_type_name` (`RscData.bus_type`/`.protocol` are
/// independent fields with no cross-validation) must not let that mismatched
/// name's own bustype defaults win -- clause 13.3.1's fixed 9600bps is
/// authoritative for this protocol regardless of whatever `bus_type_name`
/// string RscData happens to carry. Before the reordering fix, checking
/// `bustype_name` first meant `"ISO_14230_1_UART"` resolved successfully to
/// ISO14230's own defaults (10400bps), silently overriding Honda DIAG-H's
/// mandatory rate instead of leaving it at the correct 9600.
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
                                "K".to_string(),
                            ),
                        ),
                    }],
                    bus_type: Some(resource_data::BusType::BusTypeName(
                        "ISO_14230_1_UART".to_string(),
                    )),
                    protocol: Some(resource_data::Protocol::ProtocolId(
                        j2534_0404::PROTOCOL_HONDA_DIAGH_PS,
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
        "a mismatched bus_type_name must not override Honda DIAG-H's own \
         fixed, non-client-configurable 9600bps baud rate"
    );

    server.shutdown().await;
}

/// Item 5: connecting the raw `HONDA_DIAGH_PS` id directly with explicit
/// `dlc_pin_data` selecting any OTHER single pin (pin 6, well-formed but not
/// one of clause 13.2.4's two documented pins) is rejected -- the two-value
/// closed-set check, not an open choice.
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
                resource_with_protocol_id_and_pins(
                    j2534_0404::PROTOCOL_HONDA_DIAGH_PS,
                    &[(6, "K")],
                ),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "selecting pin 6 (well-formed, but not one of clause 13.2.4's two documented pins) \
             must be rejected",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("pin 1 or pin 14"),
        "the rejection should explain clause 13.2.4's two documented pins: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.connect_count(),
        0,
        "the rejected connect must never reach the native PassThruConnect call"
    );

    server.shutdown().await;
}

/// Item 6: supplying a SECOND (secondary) pin alongside a valid primary must
/// be rejected, not silently packed via the general clause-6 two-pin path --
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
                    j2534_0404::PROTOCOL_HONDA_DIAGH_PS,
                    &[(1, "K"), (14, "K")],
                ),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "supplying a secondary pin for the single-wire HONDA_DIAGH_PS bus must be rejected, \
             not silently packed as a two-pin CONFIG_J1962_PINS value",
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

/// Item 7: `SetComParam`/`GetComParam(CP_Loopback/P1_MAX/P3_MIN/P4_MIN, ...)`
/// (clause 13.3.3.1's closed parameter list) are accepted;
/// `SetComParam`/`GetComParam(CP_Baudrate, ...)` and
/// `SetComParam`/`GetComParam(CP_InitializationSettings, ...)` are rejected
/// with `PDU_ERR_COMPARAM_NOT_SUPPORTED`. `CP_Baudrate` (`DATA_RATE`) is the
/// key contrast with UART Echo Byte (`uart_echo_byte.rs`'s
/// `comparam_allowlist_is_limited_to_universal_params`, which accepts it as
/// a universal param) -- the one meaningful behavioral difference between
/// the two protocols: clause 13's baud rate is a fixed 9600bps (Table 40),
/// not `SetComParam`-configurable at all, unlike UART Echo Byte's own
/// `DATA_RATE` (clause 12.3.4.1).
#[tokio::test]
#[serial]
async fn comparam_allowlist_excludes_data_rate_and_init_settings() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        HONDA_DIAGH_RESOURCE_ID,
        &[],
    )
    .await;

    // Clause 13.3.3.1's own closed parameter list is settable.
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::LOOPBACK, 0).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P1_MAX, 20_000).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P3_MIN, 55_000).await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P4_MIN, 5_000).await;

    for &(param_id, param_name) in &[
        (j2534_0404::DATA_RATE, "CP_Baudrate"),
        (CP_INITIALIZATION_SETTINGS, "CP_InitializationSettings"),
    ] {
        let set_status = client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(param_item::Id::ParamId(param_id)),
                    com_param_class: PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(0)),
                }),
            })
            .await
            .expect_err(&format!(
                "{param_name} should be rejected on a Honda DIAG-H link -- it is not in clause \
                 13.3.3.1's closed parameter list"
            ));
        assert_eq!(set_status.code(), Code::InvalidArgument);
        let set_detail = error_detail_from_status(&set_status)
            .unwrap_or_else(|| panic!("{param_name} rejection should carry an ErrorDetail"));
        assert_eq!(
            set_detail.pdu_error,
            PduError::PduErrComparamNotSupported as i32,
            "{param_name} rejection should report PDU_ERR_COMPARAM_NOT_SUPPORTED"
        );

        let get_status = client
            .get_com_param(GetComParamRequest {
                cll_handle: Some(cll_handle),
                param: Some(get_com_param_request::Param::ParamId(param_id)),
            })
            .await
            .expect_err(&format!(
                "GetComParam should reject {param_name} on a Honda DIAG-H link too"
            ));
        assert_eq!(get_status.code(), Code::InvalidArgument);
        let get_detail = error_detail_from_status(&get_status)
            .unwrap_or_else(|| panic!("{param_name} rejection should carry an ErrorDetail"));
        assert_eq!(
            get_detail.pdu_error,
            PduError::PduErrComparamNotSupported as i32,
            "{param_name} rejection should report PDU_ERR_COMPARAM_NOT_SUPPORTED"
        );
    }

    server.shutdown().await;
}

/// Regression test (Codex review, PR #63 round 3): `P1_MAX`/`P3_MIN`/
/// `P4_MIN` are in clause 13.3.3.1's closed ComParam allowlist (Item 7
/// above), but `comparam_id::ComParamId::to_j2534_config_id` only recognized
/// `ISO9141`/`ISO14230` for this shared translation -- `base_protocol_id`
/// does not collapse `HONDA_DIAGH_PS` onto either, so `apply_j2534_params`
/// silently dropped every staged value instead of forwarding it via
/// `PassThruIoctl(SET_CONFIG)`. Connects (seeding the default `P1_MAX =
/// 20_000`us via `honda_diagh_uart()`), then `SetComParam`s a distinct
/// `P3_MIN` value and promotes it to Active via `CoptUpdateparam`
/// (`promote_via_update_param`, ADR-068's Working/Active split -- a plain
/// post-connect `SetComParam` alone only stages Working, it does not itself
/// push to hardware), asserting both actually reached the native adapter
/// with the correct 1us->0.5ms unit conversion (ADR-072).
#[tokio::test]
#[serial]
async fn p1_max_p3_min_p4_min_reach_the_native_adapter_via_set_config() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        HONDA_DIAGH_RESOURCE_ID,
        &[],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P1_MAX),
        40,
        "the seeded default P1_MAX (20_000us) should reach the native adapter as 40 \
         (0.5ms-resolution) units at connect time -- a dropped translation would leave this at \
         the mock's uninitialized 0"
    );

    set_com_param_unum32(&mut client, cll_handle, j2534_0404::P3_MIN, 60_000).await;
    promote_via_update_param(&mut client, cll_handle).await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, j2534_0404::P3_MIN),
        120,
        "a SetComParam(P3_MIN) promoted to Active via CoptUpdateparam should reach the native \
         adapter via SET_CONFIG, not be silently accepted and stored without ever forwarding"
    );

    server.shutdown().await;
}

/// Waits for the next `PduCopstFinished` on an already-open event stream
/// (mirrors `startcomm_optional_message_tx.rs`'s helper of the same name).
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

/// Waits for the next `PduCllstCommStarted` on an already-open event stream.
async fn wait_for_cll_comm_started(
    events: &mut tonic::Streaming<vci_service_interface::EventNotification>,
) {
    assert!(
        wait_for_event(events, 2000, |item| matches!(
            item.data,
            Some(event_item::Data::CllStatus(status))
                if status == vci_service_interface::PduComLogicalLinkStatus::PduCllstCommStarted as i32
        ))
        .await,
        "expected a PduCllstCommStarted event"
    );
}

/// Item 8 (ADR-174 Decision 5's key behavioral proof): `CoptStartcomm` with a
/// non-empty `cop_data` on a connected Honda DIAG-H link transmits it as a
/// genuine optional message (ISO 22900-2 §9.2.6.3.2 b) via
/// `resolve_send_recv_tx`, exactly like CAN/J1850/SCI (mirrors
/// `startcomm_optional_message_tx.rs`'s `j1850_optional_message_transmits_
/// and_reaches_comm_started`) -- NOT treated as a 5-baud/fast-init address
/// the way UART Echo Byte's own `cop_data` is (ADR-170 Decision 8, `is_kline
/// == true`). The written data is the raw `cop_data` bytes verbatim (no
/// header: clause 13 defines no CAN/J1850/KWP-style addressing,
/// `tx_header::build_tx_message`'s passthrough fallback applies, same as
/// SCI), and neither `five_baud_init_count()` nor `fast_init_count()` ever
/// increments -- proving no native init IOCTL ran at all.
#[tokio::test]
#[serial]
async fn startcomm_with_optional_message_transmits_as_a_genuine_message_not_an_init_address() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        HONDA_DIAGH_RESOURCE_ID,
        &[],
    )
    .await;

    let mut events = client
        .subscribe_event(vci_service_interface::SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await
        .expect("subscribe_event should succeed")
        .into_inner();

    let optional_message = vec![0xAA, 0xBB, 0xCC];
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: optional_message.clone(),
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
        .expect("start_com_primitive(CoptStartcomm, optional cop_data) should succeed");

    wait_for_cll_comm_started(&mut events).await;
    wait_for_cop_finished(&mut events).await;

    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        1,
        "the optional CoptStartcomm message must transmit exactly once"
    );
    assert_eq!(
        server.backdoor.written_data(MOCK_CHANNEL_ID, 0),
        optional_message,
        "Honda DIAG-H has no addressing header -- the written data should be cop_data verbatim"
    );
    assert_eq!(
        server.backdoor.five_baud_init_count(),
        0,
        "is_kline == false for this protocol -- cop_data must never be treated as a 5-baud \
         init address"
    );
    assert_eq!(
        server.backdoor.fast_init_count(),
        0,
        "is_kline == false for this protocol -- cop_data must never be treated as a fast-init \
         address"
    );

    drop(events);
    server.shutdown().await;
}

/// Item 9 (SAE J2534-2 clause 13.4.3 Table 48): a `CoptStartcomm` optional
/// message outside the 3..=255-byte TX range is rejected synchronously, via
/// the same `resolve_send_recv_tx`/`tx_message_size_range` validation a
/// `CoptSendrecv` message gets (ADR-174 Decision 5's "one resolver, one
/// behavior" point) -- a 2-byte `cop_data` is below the 3-byte minimum.
/// (The RX-side 1..=255 half of Table 48 has no functional validation path
/// to exercise here -- this codebase has no RX-side message-size check for
/// any protocol; `PassThruReadMsgs` results are forwarded as-is, matching
/// `protocol.rs`'s own doc comment on `tx_message_size_range` and mirroring
/// UART Echo Byte's identical precedent.)
#[tokio::test]
#[serial]
async fn startcomm_with_an_undersized_optional_message_is_rejected() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        HONDA_DIAGH_RESOURCE_ID,
        &[],
    )
    .await;

    let status = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![0xAA, 0xBB],
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
        .expect_err(
            "a 2-byte optional message is below clause 13.4.3 Table 48's 3-byte TX minimum and \
             must be rejected",
        );
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status
            .message()
            .contains("outside the valid TX message size range (3..=255 bytes)"),
        "unexpected error message: {}",
        status.message()
    );
    assert_eq!(
        server.backdoor.written_count(MOCK_CHANNEL_ID),
        0,
        "the rejected message must never reach the native PassThruWriteMsgs call"
    );

    server.shutdown().await;
}

/// Resolves a `PDU_IOCTL_*` shortname to its numeric `io_ctrl_command_id`
/// via `GetObjectId(OBJT_IO_CTRL, ...)` -- same per-file local helper shape
/// as `repeat_message.rs`'s/`uart_echo_byte.rs`'s own (not shared via
/// `harness.rs`, matching this codebase's existing convention).
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

/// Issues `IoCtl` against a `cll_handle` with an optional `input_data` and
/// `has_output` flag, returning the raw `output_data` on success. Mirrors
/// `repeat_message.rs`'s own `io_ctl_cll`.
async fn io_ctl_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
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

fn unum32_input(value: u32) -> DataItem {
    DataItem {
        data: Some(data_item::Data::Unum32Value(value)),
    }
}

/// edge-case-hunter finding (Phase 10 verification pass): ADR-174 Decision 7
/// and `implementation-notes.md` assert Repeat Messaging "needs no new
/// rejection" for this protocol -- clause 13 has no exclusion equivalent to
/// UART Echo Byte's clause 12.3.3.1 (contrast
/// `uart_echo_byte.rs::repeat_messaging_is_rejected_on_all_three_commands`),
/// so the existing generic `START`/`QUERY`/`STOP_REPEAT_MESSAGE` forwarding
/// should already work end-to-end via `tx_header::response_header_bytes`'s
/// generic no-header fallback (this protocol has no addressing concept at
/// all, clause 13.4.3 -- unlike `repeat_message.rs`'s CAN-based tests, no
/// `set_can_addressing`-style `UniqueRespIdTable` setup is needed or
/// possible here). That claim was previously verified only by spec/code
/// reading, not by a test -- this closes the gap with a direct smoke test:
/// `START` (condition 0, unconditional continued retransmission -- the same
/// default `repeat_message.rs` uses) succeeds and returns a `MsgId`,
/// `QUERY` on that `MsgId` reports status 1 (still live, ADR-173 Table 53
/// polarity), and `STOP` succeeds.
#[tokio::test]
#[serial]
async fn repeat_messaging_start_query_stop_succeeds_on_a_connected_link() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        HONDA_DIAGH_RESOURCE_ID,
        &[],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;
    let query_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_QUERY_REPEAT_MESSAGE").await;
    let stop_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_STOP_REPEAT_MESSAGE").await;

    // mask_data/pattern_data go through the same TX message-size validation
    // (clause 13.4.3 Table 48's 3..=255 byte range) as repeat_msg_data
    // itself, since both are resolved via the same `resolve_send_recv_tx`
    // path this protocol's optional-message handling uses -- an empty mask
    // (0 bytes) is below the 3-byte minimum, so both are given 3 bytes here.
    let setup = DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup(
                1000,
                0,
                &[0x01, 0x02, 0x03],
                &[0x00, 0x00, 0x00],
                &[0xFF, 0xFF, 0xFF],
                &[],
            ),
        })),
    };
    let start_output = io_ctl_cll(&mut client, cll_handle, start_id, Some(setup), true)
        .await
        .expect(
            "PDU_IOCTL_START_REPEAT_MESSAGE should succeed on a connected, opted-in Honda \
             DIAG-H link -- clause 13 has no Repeat Messaging exclusion",
        );
    let msg_id = match start_output.and_then(|d| d.data) {
        Some(data_item::Data::Unum32Value(msg_id)) => msg_id,
        other => panic!(
            "PDU_IOCTL_START_REPEAT_MESSAGE should return a Unum32Value MsgId, got {other:?}"
        ),
    };

    let query_output = io_ctl_cll(
        &mut client,
        cll_handle,
        query_id,
        Some(unum32_input(msg_id)),
        true,
    )
    .await
    .expect("PDU_IOCTL_QUERY_REPEAT_MESSAGE should succeed for a MsgId this CLL just started");
    let status = match query_output.and_then(|d| d.data) {
        Some(data_item::Data::Unum32Value(status)) => status,
        other => panic!(
            "PDU_IOCTL_QUERY_REPEAT_MESSAGE should return a Unum32Value status, got {other:?}"
        ),
    };
    assert_eq!(
        status, 1,
        "the mock reports status 1 (\"live\") for a still-live slot (ADR-173 Table 53 polarity)"
    );

    io_ctl_cll(
        &mut client,
        cll_handle,
        stop_id,
        Some(unum32_input(msg_id)),
        false,
    )
    .await
    .expect("PDU_IOCTL_STOP_REPEAT_MESSAGE should succeed for a MsgId this CLL owns");

    server.shutdown().await;
}

/// edge-case-hunter finding: `ioctl_start_repeat_message`'s generic
/// `.min(12)` fallback arm (`rpc_misc.rs`) -- covering every protocol other
/// than raw CAN, ISO15765, and (post-ADR-186) J1939 -- had zero direct test
/// coverage despite narrowing this protocol's own ordinary TX range
/// (clause 13.4.3 Table 48: `3..=255`) down to `3..=12`. This protocol has
/// no header (clause 13.4.3, `tx_header::build_tx_message`'s generic
/// no-header fallback), so `repeat_msg_data.len()` IS `full_message.len()`
/// directly, making the boundary trivial to pin: a 12-byte payload is
/// exactly SAE J2534-1 v04.04 §7.2.7's flat periodic-message cap and must
/// be ACCEPTED; a 13-byte payload exceeds it and must be REJECTED.
#[tokio::test]
#[serial]
async fn repeat_messaging_start_accepts_datasize_12_and_rejects_datasize_13() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let cll_handle = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        HONDA_DIAGH_RESOURCE_ID,
        &[],
    )
    .await;

    let start_id = resolve_ioctl_id(&mut client, "PDU_IOCTL_START_REPEAT_MESSAGE").await;

    let accepted_setup = DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup(1000, 0, &[0x01; 12], &[0x00; 12], &[0xFF; 12], &[]),
        })),
    };
    io_ctl_cll(
        &mut client,
        cll_handle,
        start_id,
        Some(accepted_setup),
        true,
    )
    .await
    .expect(
        "a 12-byte repeat_msg_data is exactly ADR-186's periodic-message cap for this \
             protocol (SAE J2534-1 v04.04 §7.2.7) and must be accepted",
    );

    let rejected_setup = DataItem {
        data: Some(data_item::Data::BytearrayData(IoBytearray {
            data: pack_repeat_message_setup(1000, 0, &[0x01; 13], &[0x00; 13], &[0xFF; 13], &[]),
        })),
    };
    let err = io_ctl_cll(
        &mut client,
        cll_handle,
        start_id,
        Some(rejected_setup),
        true,
    )
    .await
    .expect_err(
        "a 13-byte repeat_msg_data exceeds ADR-186's periodic-message cap for this \
             protocol and must be rejected",
    );
    assert_eq!(err.code(), Code::InvalidArgument);

    server.shutdown().await;
}

// ── ADR-208: SAE J2534-2 clause 7 Additional Channels for Honda DIAG-H ─────

/// Mirrors `uart_echo_byte.rs`'s own
/// `uart_echo_byte_ch1_establishes_independently_alongside_a_uart_echo_byte_ps_sibling`
/// (ADR-207)/`gm_uart.rs`'s/`j1939.rs`'s own equivalents (ADR-206): a
/// directly-named `PROTOCOL_HONDA_DIAGH_CH1` id establishes its own physical
/// channel independently of a `PROTOCOL_HONDA_DIAGH_PS` sibling on the same
/// module -- confirms `resources::chx_block_base`'s new
/// `PROTOCOL_HONDA_DIAGH_PS => Some(PROTOCOL_HONDA_DIAGH_CH1)` entry resolves
/// end-to-end, and that a `_CHx` id (no J1962 pin concept at all) connects
/// with no `dlc_pin_data`.
#[tokio::test]
#[serial]
async fn honda_diagh_ch1_establishes_independently_alongside_a_honda_diagh_ps_sibling() {
    let server = start_j2534_2_server().await;
    let mut client = server.client().await;

    let ps_cll = create_and_connect_cll_for_module(
        &mut client,
        MOCK_MODULE_HANDLE,
        HONDA_DIAGH_RESOURCE_ID,
        &[],
    )
    .await;
    assert_eq!(server.backdoor.connect_count(), 1);
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID),
        j2534_0404::PROTOCOL_HONDA_DIAGH_PS
    );

    let chx_cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_HONDA_DIAGH_CH1, &[]), // _CH1
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
            "connect_com_logical_link should succeed for a directly-named \
             PROTOCOL_HONDA_DIAGH_CH1 id",
        );

    assert_eq!(
        server.backdoor.connect_count(),
        2,
        "the _PS sibling and its own _CH1 Additional Channel should open two distinct physical \
         channels"
    );
    assert_eq!(
        server.backdoor.channel_protocol_id(MOCK_CHANNEL_ID + 1),
        j2534_0404::PROTOCOL_HONDA_DIAGH_CH1,
        "the second connect should open a native PROTOCOL_HONDA_DIAGH_CH1 channel"
    );

    let _ = ps_cll;
    server.shutdown().await;
}

/// Regression test: before the mechanical extension of ADR-211's/ADR-212's
/// established pattern (`resources.rs::chx_device_info_supported_
/// parameter` gaining a Honda DIAG-H guard arm), this function returned
/// `None` for every Honda DIAG-H id, so `check_chx_capacity`'s
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
/// it. Simplified relative to that FT-CAN test since Honda DIAG-H shares
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

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_id_and_pins(j2534_0404::PROTOCOL_HONDA_DIAGH_CH1 + 1, &[]), // _CH2
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
         parameter returned None for Honda DIAG-H (the bug this fix corrects)"
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

/// Mirrors `uart_echo_byte.rs`'s own `connecting_via_compound_chx_name_succeeds`
/// (ADR-207)/`gm_uart.rs`'s/`j1939.rs`'s own equivalents (ADR-206):
/// connecting via the compound `_CHx`-suffixed `protocol_name` grammar
/// ("HONDA_DIAGH_CH1") succeeds the same way the raw-id route above does --
/// this is the test that specifically proves the `names.rs`
/// `requested_index.is_some()` bypass fix (ADR-208 Decision item 5) works:
/// without it, this connect would fail with the clause-6/clause-7
/// mutual-exclusion rejection instead.
#[tokio::test]
#[serial]
async fn honda_diagh_connecting_via_compound_chx_name_succeeds() {
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
                        "HONDA_DIAGH_CH1".to_string(),
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
        j2534_0404::PROTOCOL_HONDA_DIAGH_CH1,
        "the compound-name route should open a native PROTOCOL_HONDA_DIAGH_CH1 channel, same as \
         the raw protocol-id route"
    );

    server.shutdown().await;
}

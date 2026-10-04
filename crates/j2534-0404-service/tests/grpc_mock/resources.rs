//! `GetResourceIds`/`CreateComLogicalLink` against the ISO 22900-2-style
//! static resource table (`service::resources`): selector filtering (protocol
//! id/name, bus type id/name, pin type/number) and resource-ID-based
//! `CreateComLogicalLink` resolution, including the legacy raw/extended
//! `ChannelProtocol` fallback path.

use serial_test::serial;
use vci_service_interface::{
    CreateComLogicalLinkRequest, GetConflictingResourcesRequest, GetResourceIdsRequest,
    GetResourceStatusRequest, ModuleAndResourceId, ModuleData, ModuleHandle, ModuleItem, PduError,
    PinData, ResourceData, create_com_logical_link_request, error_detail_from_status,
    get_conflicting_resources_request, module_and_resource_id, resource_data,
};

use crate::harness::*;

/// Issues `GetResourceIds` with the given selector and returns the resolved
/// resource IDs in table order.
async fn get_resource_ids(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource_data: ResourceData,
) -> Vec<u32> {
    client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource_data: Some(resource_data),
        })
        .await
        .expect("get_resource_ids should succeed")
        .into_inner()
        .resource_id_list
        .and_then(|list| list.resource_id_data_array.into_iter().next())
        .map(|data| data.resource_id_array)
        .unwrap_or_default()
}

fn no_filter() -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![],
        bus_type: None,
        protocol: None,
    }
}

fn by_bustype_name(name: &str) -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![],
        bus_type: Some(resource_data::BusType::BusTypeName(name.to_string())),
        protocol: None,
    }
}

fn by_protocol_name(name: &str) -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![],
        bus_type: None,
        protocol: Some(resource_data::Protocol::ProtocolName(name.to_string())),
    }
}

fn by_pin_number(pin_number: u32) -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![PinData {
            dlc_pin_number: pin_number,
            dlc_pin_type: None,
        }],
        bus_type: None,
        protocol: None,
    }
}

/// A typed pin selector: `(pin_number, pin_type_name)`.
fn by_pin(pin_number: u32, pin_type_name: &str) -> ResourceData {
    ResourceData {
        dlc_pin_data: vec![PinData {
            dlc_pin_number: pin_number,
            dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                pin_type_name.to_string(),
            )),
        }],
        bus_type: None,
        protocol: None,
    }
}

/// Multiple typed pin selectors at once -- every entry is conjunctive
/// (`by_pin`'s single-entry shape, repeated).
fn by_pins(pins: &[(u32, &str)]) -> ResourceData {
    ResourceData {
        dlc_pin_data: pins
            .iter()
            .map(|&(pin_number, pin_type_name)| PinData {
                dlc_pin_number: pin_number,
                dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                    pin_type_name.to_string(),
                )),
            })
            .collect(),
        bus_type: None,
        protocol: None,
    }
}

fn by_protocol_name_with_pins(name: &str, pins: &[(u32, &str)]) -> ResourceData {
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
        protocol: Some(resource_data::Protocol::ProtocolName(name.to_string())),
    }
}

/// A one-entry `input_module_list` naming the single supported module handle
/// -- the minimum non-empty `GetConflictingResourcesRequest.input_module_list`
/// needed to get a non-empty conflict result (ADR-106).
fn single_module_list() -> ModuleItem {
    ModuleItem {
        module_data: vec![ModuleData {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            ..Default::default()
        }],
    }
}

/// The full table in resource-ID order (`resources::resource_table()`'s 97
/// rows, 0x0201..=0x0261, contiguous with no gaps -- see
/// `j2534-0404-service/src/service/resources.rs`). Rows 0x0226-0x022F are
/// the SAE J2534-2 clause 9 Single Wire CAN (SWCAN/GMLAN, ADR-164/Phase 4)
/// rows; rows 0x0230-0x0239 are the SAE J2534-2 clause 20 Fault-Tolerant CAN
/// (ISO 11898-3, ADR-168/Phase 6) rows; row 0x023A is the SAE J2534-2 clause
/// 12 UART Echo Byte Protocol (ADR-170/Phase 9) row; row 0x023B is the SAE
/// J2534-2 clause 13 Honda DIAG-H Protocol (ADR-174/Phase 10) row; row
/// 0x023C is the SAE J2534-2 clause 17 SAE J1708 Protocol (ADR-175/Phase 11)
/// row; rows 0x023D-0x025C are the SAE J2534-2 clause 10 Analog Inputs
/// (ADR-177/Phase 15) rows; rows 0x025D-0x025E are the SAE J2534-2 clause 16
/// SAE J1939 Protocol (ADR-179/Phase 5) rows; row 0x025F is the SAE J2534-2
/// clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a) row; row 0x0260 is
/// the SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8) row; row
/// 0x0261 is the SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16) row.
const ALL_RESOURCE_IDS: [u32; 97] = [
    0x0201, 0x0202, 0x0203, 0x0204, 0x0205, 0x0206, 0x0207, 0x0208, 0x0209, 0x020A, 0x020B, 0x020C,
    0x020D, 0x020E, 0x020F, 0x0210, 0x0211, 0x0212, 0x0213, 0x0214, 0x0215, 0x0216, 0x0217, 0x0218,
    0x0219, 0x021A, 0x021B, 0x021C, 0x021D, 0x021E, 0x021F, 0x0220, 0x0221, 0x0222, 0x0223, 0x0224,
    0x0225, 0x0226, 0x0227, 0x0228, 0x0229, 0x022A, 0x022B, 0x022C, 0x022D, 0x022E, 0x022F, 0x0230,
    0x0231, 0x0232, 0x0233, 0x0234, 0x0235, 0x0236, 0x0237, 0x0238, 0x0239, 0x023A, 0x023B, 0x023C,
    0x023D, 0x023E, 0x023F, 0x0240, 0x0241, 0x0242, 0x0243, 0x0244, 0x0245, 0x0246, 0x0247, 0x0248,
    0x0249, 0x024A, 0x024B, 0x024C, 0x024D, 0x024E, 0x024F, 0x0250, 0x0251, 0x0252, 0x0253, 0x0254,
    0x0255, 0x0256, 0x0257, 0x0258, 0x0259, 0x025A, 0x025B, 0x025C, 0x025D, 0x025E, 0x025F, 0x0260,
    0x0261,
];

#[tokio::test]
#[serial]
async fn unfiltered_get_resource_ids_returns_all_97_ids_in_table_order() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let ids = get_resource_ids(&mut client, no_filter()).await;
    assert_eq!(ids, ALL_RESOURCE_IDS.to_vec());

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn filters_by_bustype_name() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let can_ids = get_resource_ids(&mut client, by_bustype_name("ISO_11898_2_DWCAN")).await;
    assert_eq!(
        can_ids,
        vec![
            0x0201, 0x0202, 0x0203, 0x0204, 0x0205, 0x0206, 0x0207, 0x0208, 0x0209, 0x020A
        ]
    );

    // Spec correction: bus 0x0308 now carries 8 rows -- four
    // SAE_J2610_on_SAE_J2610_SCI configurations plus the four bare
    // SAE_J2610_SCI configurations.
    let sci_ids = get_resource_ids(&mut client, by_bustype_name("SAE_J2610_UART")).await;
    assert_eq!(
        sci_ids,
        vec![
            0x021E, 0x021F, 0x0220, 0x0221, 0x0222, 0x0223, 0x0224, 0x0225
        ]
    );

    let combined_kline_ids = get_resource_ids(
        &mut client,
        by_bustype_name("ISO_9141_2_UART_and_ISO_14230_1_UART"),
    )
    .await;
    assert_eq!(combined_kline_ids, vec![0x0212, 0x0213, 0x0214]);

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn filters_by_protocol_name() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    assert_eq!(
        get_resource_ids(&mut client, by_protocol_name("ISO_OBD_on_K_Line")).await,
        vec![0x0213]
    );

    // Spec correction: SAE_J2610_SCI's four configuration rows are renumbered
    // 0x0222-0x0225.
    assert_eq!(
        get_resource_ids(&mut client, by_protocol_name("SAE_J2610_SCI")).await,
        vec![0x0222, 0x0223, 0x0224, 0x0225]
    );

    assert_eq!(
        get_resource_ids(&mut client, by_protocol_name("SCI_A_ENGINE")).await,
        vec![0x0222]
    );

    // Spec correction: the four SAE_J2610_on_SAE_J2610_SCI configuration
    // rows, matched by their shared protocol_name (not config_name, which
    // they don't set).
    assert_eq!(
        get_resource_ids(&mut client, by_protocol_name("SAE_J2610_on_SAE_J2610_SCI")).await,
        vec![0x021E, 0x021F, 0x0220, 0x0221]
    );

    // Legacy alias name (not a table protocol_name): resolves via
    // `map_protocol_name` to `ChannelProtocol::ISO15765`, which (ADR-164/
    // Phase 4, extended by ADR-168/Phase 6) now matches three rows sharing
    // that identity: the native dual-wire `ISO_15765_2` row (0x0206), its
    // SWCAN sibling `ISO_15765_2_SWCAN` (0x022B), and its FTCAN sibling
    // `ISO_15765_2_FTCAN` (0x0235) -- `GetResourceIds` is a discovery/
    // enumeration RPC, so listing every distinct physical resource is
    // correct, not a regression (see `names.rs`'s own unit test of the same
    // correction).
    assert_eq!(
        get_resource_ids(&mut client, by_protocol_name("ISO15765")).await,
        vec![0x0206, 0x022B, 0x0235]
    );

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn filters_by_bustype_id_and_protocol_name_combined() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let matching = ResourceData {
        dlc_pin_data: vec![],
        bus_type: Some(resource_data::BusType::BusTypeId(0x0301)),
        protocol: Some(resource_data::Protocol::ProtocolName(
            "ISO_15765_2".to_string(),
        )),
    };
    assert_eq!(get_resource_ids(&mut client, matching).await, vec![0x0206]);

    // Contradictory combo: ISO_15765_2 only lives on bus 0x0301, not 0x0302.
    let contradictory = ResourceData {
        dlc_pin_data: vec![],
        bus_type: Some(resource_data::BusType::BusTypeId(0x0302)),
        protocol: Some(resource_data::Protocol::ProtocolName(
            "ISO_15765_2".to_string(),
        )),
    };
    assert!(
        get_resource_ids(&mut client, contradictory)
            .await
            .is_empty()
    );

    server.shutdown().await;
}

/// P2 fix: a legacy numeric `bus_type_id` that doesn't match any table
/// `bus_type_id` falls back to matching a row's *fixed* J2534
/// connect-protocol ID (`names::J2534Service::legacy_bustype_hw_id`). For a
/// native SCI hardware id (e.g. `SCI_A_ENGINE`, `0x07`), that must include
/// both the bare `SAE_J2610_SCI` row that natively identifies as
/// `SCI_A_ENGINE` (`0x0222`) *and* the `SAE_J2610_on_SAE_J2610_SCI` row that
/// overrides its connect protocol to `SCI_A_ENGINE` (`0x021E`) -- both rows
/// actually issue `PassThruConnect(SCI_A_ENGINE, ...)`, so a legacy hw-id
/// filter must find both, not just the row whose own `ChannelProtocol`
/// happens to equal it. `SCI_MODE` itself (`0x0B`, the shared identity of
/// the four `_on_` rows) is no longer any row's fixed connect protocol at
/// all (ADR-069's pin-typing amendment) and so now correctly matches none.
#[tokio::test]
#[serial]
async fn legacy_sci_bustype_id_matches_rows_by_effective_connect_protocol() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let sci_a_engine_hw_id = ResourceData {
        dlc_pin_data: vec![],
        bus_type: Some(resource_data::BusType::BusTypeId(j2534_0404::SCI_A_ENGINE)),
        protocol: None,
    };
    assert_eq!(
        get_resource_ids(&mut client, sci_a_engine_hw_id).await,
        vec![0x021E, 0x0222],
        "a legacy SCI_A_ENGINE hw id should match every row that actually connects with it, \
         not just the row whose own ChannelProtocol equals it"
    );

    let sci_mode_hw_id = ResourceData {
        dlc_pin_data: vec![],
        bus_type: Some(resource_data::BusType::BusTypeId(j2534_0404::SCI_MODE)),
        protocol: None,
    };
    assert!(
        get_resource_ids(&mut client, sci_mode_hw_id)
            .await
            .is_empty(),
        "SCI_MODE is no longer used as a connect protocol by any table row, so a legacy \
         SCI_MODE hw id should now match none"
    );

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn filters_by_pin_number() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Bare pin number 6, no type: matches every row with a pin numbered 6 --
    // the ten ISO_11898_2_DWCAN rows, the two SCI_A_ENGINE configurations
    // (SAE_J2610_on_SAE_J2610_SCI's 0x021E and SAE_J2610_SCI's 0x0222),
    // since pin 6 is TX on that SCI configuration (spec correction), and
    // (ADR-188/Phase 7 Stage 7a) the TP2.0 row 0x025F, which reuses the same
    // pin 6/14 pair as dual-wire CAN.
    let pin6_ids = get_resource_ids(&mut client, by_pin_number(6)).await;
    assert_eq!(
        pin6_ids,
        vec![
            0x0201, 0x0202, 0x0203, 0x0204, 0x0205, 0x0206, 0x0207, 0x0208, 0x0209, 0x020A, 0x021E,
            0x0222, 0x025F,
        ]
    );

    server.shutdown().await;
}

/// Typed pin selectors (spec correction): a pin type disambiguates rows that
/// share a bare pin number across buses.
#[tokio::test]
#[serial]
async fn filters_by_typed_pin() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // (6, HI): the CAN rows and (ADR-188/Phase 7 Stage 7a) the TP2.0 row
    // 0x025F, which types pin 6 the same way -- excludes the SCI_A_ENGINE
    // rows, which also use pin 6 but typed TX, not HI.
    let can_only = get_resource_ids(&mut client, by_pin(6, "HI")).await;
    assert_eq!(
        can_only,
        vec![
            0x0201, 0x0202, 0x0203, 0x0204, 0x0205, 0x0206, 0x0207, 0x0208, 0x0209, 0x020A, 0x025F
        ]
    );

    // (6, TX): only the two SCI_A_ENGINE rows.
    let sci_a_engine_only = get_resource_ids(&mut client, by_pin(6, "TX")).await;
    assert_eq!(sci_a_engine_only, vec![0x021E, 0x0222]);

    // (2, PLUS): every PWM/VPW/SAE_J1850 row -- pin 2 is always PLUS on all
    // three J1850 buses.
    let plus_pin = get_resource_ids(&mut client, by_pin(2, "PLUS")).await;
    assert_eq!(
        plus_pin,
        vec![
            0x0215, 0x0216, 0x0217, 0x0218, 0x0219, 0x021A, 0x021B, 0x021C, 0x021D
        ]
    );

    // (10, MINUS): PWM and SAE_J1850 rows only -- excludes the pure-VPW
    // rows, which have no pin 10 at all.
    let minus_pin = get_resource_ids(&mut client, by_pin(10, "MINUS")).await;
    assert_eq!(
        minus_pin,
        vec![0x0215, 0x0216, 0x0217, 0x021A, 0x021C, 0x021D]
    );

    server.shutdown().await;
}

/// SAE J2534-2 clause 24 Table 103 (ADR-194/Phase 16, Codex review finding
/// on PR #102): Ethernet_NDIS (0x0261)'s typed `dlc_pins` carries only
/// Option 1's Tx pins (3 = PLUS, 11 = MINUS) -- Option 2's alternate Tx
/// pins (1 = PLUS, 9 = MINUS) are invisible to a plain `dlc_pins` lookup,
/// so a caller filtering `GetResourceIds` by either of Option 2's pins
/// would never discover this row at all, even though a real connection
/// can select Option 2 via `CP_NdisPinOption`. No other resource row types
/// pin 1 as PLUS or pin 9 as MINUS, so each of these typed queries must
/// resolve to exactly this row.
#[tokio::test]
#[serial]
async fn filters_by_ethernet_ndis_option_2_alternate_pin() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let by_alternate_plus = get_resource_ids(&mut client, by_pin(1, "PLUS")).await;
    assert_eq!(by_alternate_plus, vec![0x0261]);

    let by_alternate_minus = get_resource_ids(&mut client, by_pin(9, "MINUS")).await;
    assert_eq!(by_alternate_minus, vec![0x0261]);

    // A mismatched type at the same pin number must NOT match -- pin 1 as
    // HI (SAE_J2411_SWCAN/ISO_11898_3_DWFTCAN's own typing) is a different
    // logical pin, not Ethernet_NDIS's alternate Tx(+).
    let by_wrong_type = get_resource_ids(&mut client, by_pin(1, "HI")).await;
    assert!(
        !by_wrong_type.contains(&0x0261),
        "pin 1 typed HI must not match Ethernet_NDIS's alternate Tx(+) (PLUS)"
    );

    server.shutdown().await;
}

/// Codex review finding, PR #102 round 6: `dlc_pin_data` entries are
/// conjunctive and must describe ONE realizable wiring -- Option 1 (pins
/// 3/11) or Option 2 (pins 1/9), never a mix of both. A caller naming
/// Option 2's Tx(+) (pin 1) together with Option 1's Tx(-) (pin 11), or
/// even two entries from the same alternate pair (pin 1 AND pin 3, both
/// typed PLUS), describes no physically realizable Ethernet_NDIS wiring
/// and must resolve to no match, even though EACH entry individually
/// would match the row if evaluated in isolation against "either option".
#[tokio::test]
#[serial]
async fn filters_reject_mixed_ethernet_ndis_pin_options() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Option 2's Tx(+) (pin 1) combined with Option 1's Tx(-) (pin 11) --
    // no single wiring has both.
    let mixed_across_options =
        get_resource_ids(&mut client, by_pins(&[(1, "PLUS"), (11, "MINUS")])).await;
    assert!(
        !mixed_across_options.contains(&0x0261),
        "Option 2's Tx(+) (pin 1) mixed with Option 1's Tx(-) (pin 11) must not match"
    );

    // Both Tx(+) pins from different options at once -- also unrealizable.
    let both_plus_pins = get_resource_ids(&mut client, by_pins(&[(1, "PLUS"), (3, "PLUS")])).await;
    assert!(
        !both_plus_pins.contains(&0x0261),
        "pin 1 and pin 3 (both PLUS, from different options) must not match together"
    );

    // Sanity check: the fully-consistent Option 1 pin set (3, 11) DOES
    // still match, confirming the rejection above is genuinely about the
    // cross-option mix, not a regression breaking ordinary multi-pin
    // queries. `SAE_J1708` (0x023C) wires the identical pins (3 = PLUS,
    // 11 = MINUS, `PINS_SAE_J1708`) and matches too -- not an
    // Ethernet_NDIS-specific effect, just this bare pin pair genuinely
    // being shared by both rows.
    let consistent_option1 =
        get_resource_ids(&mut client, by_pins(&[(3, "PLUS"), (11, "MINUS")])).await;
    assert_eq!(consistent_option1, vec![0x023C, 0x0261]);

    // Sanity check: the fully-consistent Option 2 pin set (1, 9) also
    // matches.
    let consistent_option2 =
        get_resource_ids(&mut client, by_pins(&[(1, "PLUS"), (9, "MINUS")])).await;
    assert_eq!(consistent_option2, vec![0x0261]);

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn create_com_logical_link_resolves_table_resource_id_to_expected_j2534_protocol() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // 0x0213 (ISO_OBD_on_K_Line) resolves to a ChannelProtocol whose J2534
    // hardware protocol is ISO9141 (0x03).
    let cll_handle =
        create_and_connect_cll(&mut client, 0x0213, &[(j2534_0404::DATA_RATE, 10_400)]).await;
    send_data(&mut client, cll_handle, vec![0x01, 0x02], vec![]).await;
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::ISO9141
    );

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn create_com_logical_link_resolves_sci_config_resource_id() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // 0x0225 (SAE_J2610_SCI, config SCI_B_TRANS) resolves to the SCI_B_TRANS
    // (0x0A) J2534 hardware protocol.
    let cll_handle =
        create_and_connect_cll(&mut client, 0x0225, &[(j2534_0404::DATA_RATE, 7_812)]).await;
    send_data(&mut client, cll_handle, vec![0x01, 0x02], vec![]).await;
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::SCI_B_TRANS
    );

    server.shutdown().await;
}

/// An unrecognized numeric resource id falls back to
/// `ChannelProtocol::from_raw` (the legacy behavior, unchanged by the
/// resources table): `CreateComLogicalLink` still succeeds, wrapping the raw
/// value verbatim, and only fails later where that raw value is used as a
/// J2534 hardware protocol id (e.g. `ConnectComLogicalLink`'s
/// `PassThruConnect`) -- consistent with pre-table behavior for any
/// unrecognized numeric id.
#[tokio::test]
#[serial]
async fn create_com_logical_link_accepts_unknown_numeric_id_via_legacy_from_raw_fallback() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_response = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol(0xFFFF),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed via the legacy from_raw fallback")
        .into_inner();
    assert!(cll_response.cll_handle.is_some());

    server.shutdown().await;
}

/// A combined-bus-type resource (0x0212, `ISO_15031_5_on_ISO_9141_2_and_ISO_14230_4`,
/// bus `ISO_9141_2_UART_and_ISO_14230_1_UART`) must still get non-empty
/// ComParam defaults -- in particular a nonzero `DATA_RATE` -- from the
/// matched row's canonical bus_type_name/protocol_name, not an empty Working
/// set (a connect relying on defaults would otherwise use baud 0).
#[tokio::test]
#[serial]
async fn create_com_logical_link_combined_kline_resource_gets_nonempty_comparam_defaults() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_cll(&mut client, 0x0212, 1).await;
    let data_rate = get_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE).await;
    assert_eq!(
        data_rate, 10_400,
        "combined K-line resource 0x0212 should inherit ISO_9141_2_UART's DATA_RATE default"
    );

    server.shutdown().await;
}

/// Resources 0x0202 (`ISO_14229_3`), 0x0203 (`ISO_14229_3_on_ISO_15765_2`),
/// and 0x0207 (`ISO_15765_3`) are all spec-identical to
/// `ISO_15765_3_on_ISO_15765_2` (ISO_14229_3 supersedes it with the same
/// feature set) and must get that preset's UDS/ISO-TP
/// protocol-layer defaults -- not just the ISO_11898_2_DWCAN bus defaults --
/// so a connect relying on defaults gets a real `CP_P2Max` response window
/// instead of `0`.
#[tokio::test]
#[serial]
async fn create_com_logical_link_iso_14229_3_variants_get_uds_isotp_protocol_defaults() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    for resource_id in [0x0202, 0x0203, 0x0207] {
        let cll_handle = create_cll(&mut client, resource_id, 1).await;
        let p2_max = get_com_param_unum32(&mut client, cll_handle, j2534_0404::P2_MAX).await;
        assert_eq!(
            p2_max, 150_000,
            "resource {resource_id:#06x} should inherit iso_15765_3_on_iso_15765_2's \
             CP_P2Max protocol default, not just the bus-type default (which leaves it at 0)"
        );
    }

    server.shutdown().await;
}

/// Conformance-audit finding A2-16 (ISO 22900-2:2009 Table B.19): the
/// K-line/UART, ISO_15765_3, and ISO_15765_4 protocol presets must seed
/// `CP_RC21RequestTime`/`CP_RC23RequestTime`/`CP_TesterPresentTime`/
/// `CP_P3Func`/`CP_P3Phys` at the spec-mandated values, not just an
/// un-set/zero Working entry -- checked here via `GetComParam` with no prior
/// `SetComParam`, so a regression to the old (wrong) defaults would be
/// caught even though no test previously asserted these un-set values.
#[tokio::test]
#[serial]
async fn create_com_logical_link_a2_16_comparam_defaults() {
    const CP_RC21_REQUEST_TIME: u32 = 0x8022;
    const CP_RC23_REQUEST_TIME: u32 = 0x8025;
    const CP_P3_FUNC: u32 = 0x80B3;
    const CP_P3_PHYS: u32 = 0x80B4;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    // ISO_15765_4 (resource 0x0205, ISO_15031_5_on_ISO_15765_4).
    let cll_handle = create_cll(&mut client, 0x0205, 1).await;
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_P3_FUNC).await,
        50_000,
        "ISO_15765_4 resource 0x0205 should default CP_P3Func to 50000 per Table B.19"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_P3_PHYS).await,
        50_000,
        "ISO_15765_4 resource 0x0205 should default CP_P3Phys to 50000 per Table B.19"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_RC23_REQUEST_TIME).await,
        0,
        "ISO_15765_4 resource 0x0205 should default CP_RC23RequestTime to 0 per Table B.19"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME).await,
        3_000_000,
        "ISO_15765_4 resource 0x0205 should default CP_TesterPresentTime to 3000000 per Table B.19"
    );

    // ISO_15765_3 (resource 0x0207).
    let cll_handle = create_cll(&mut client, 0x0207, 2).await;
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_P3_FUNC).await,
        50_000,
        "ISO_15765_3 resource 0x0207 should default CP_P3Func to 50000 per Table B.19"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_P3_PHYS).await,
        50_000,
        "ISO_15765_3 resource 0x0207 should default CP_P3Phys to 50000 per Table B.19"
    );

    // K-line/UART resources (ISO_14230_3, ISO_14230_4, SAE_J2190).
    for resource_id in [0x020B, 0x020D, 0x020E] {
        let cll_handle = create_cll(&mut client, resource_id, 3).await;
        assert_eq!(
            get_com_param_unum32(&mut client, cll_handle, CP_RC21_REQUEST_TIME).await,
            0,
            "K-line resource {resource_id:#06x} should default CP_RC21RequestTime to 0 per \
             Table B.19"
        );
        assert_eq!(
            get_com_param_unum32(&mut client, cll_handle, CP_RC23_REQUEST_TIME).await,
            0,
            "K-line resource {resource_id:#06x} should default CP_RC23RequestTime to 0 per \
             Table B.19"
        );
        assert_eq!(
            get_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME).await,
            3_000_000,
            "K-line resource {resource_id:#06x} should default CP_TesterPresentTime to 3000000 \
             per Table B.19"
        );
    }

    // ISO_OBD_on_K_Line (resource 0x0213) shares `kwp_on_kline_common` with
    // the three K-line resources above, but maps to ISO_9141_2, which Table
    // B.19 has no CP_RC21RequestTime/CP_TesterPresentTime row for at all --
    // it must NOT pick up the K-line correction (Codex review, PR #120: the
    // shared helper's fix silently leaked into this unrelated, unverified
    // resource until an explicit override was restored).
    let cll_handle = create_cll(&mut client, 0x0213, 4).await;
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_RC21_REQUEST_TIME).await,
        200_000,
        "ISO_OBD_on_K_Line resource 0x0213 should keep its original CP_RC21RequestTime \
         (200000) -- Table B.19 has no ISO_9141_2 row to justify changing it"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_RC23_REQUEST_TIME).await,
        200_000,
        "ISO_OBD_on_K_Line resource 0x0213 should keep its original CP_RC23RequestTime \
         (200000) -- Table B.19 has no ISO_9141_2 row to justify changing it"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME).await,
        2_000_000,
        "ISO_OBD_on_K_Line resource 0x0213 should keep its original CP_TesterPresentTime \
         (2000000) -- Table B.19 has no ISO_9141_2 row to justify changing it"
    );

    server.shutdown().await;
}

/// Conformance-audit finding A2-16 follow-up (ISO 22900-2:2009 Table B.19):
/// the J1850 (`SAE_J1850_VPW`/`SAE_J1850_PWM`) protocol presets, all seeded
/// via the shared `j1850_common` helper, must default `CP_RC21RequestTime`/
/// `CP_RC23RequestTime` to `0` and `CP_TesterPresentTime` to `3000000` -- not
/// the stale `200000`/`2000000` values previously hardcoded here, which had
/// no Table B.19 basis for J1850 (the same bug class A2-16 already fixed
/// elsewhere). Covers both bus variants (PWM and VPW) and both protocol
/// families (ISO_15031_5 and SAE_J2190) via a representative subset of
/// resources; excludes the bare native `SAE_J1850_PWM`/`SAE_J1850_VPW`
/// channels (0x0216/0x0219), which get their ComParam defaults from
/// `bustype_default_params` instead.
#[tokio::test]
#[serial]
async fn create_com_logical_link_a2_16_j1850_comparam_defaults() {
    const CP_RC21_REQUEST_TIME: u32 = 0x8022;
    const CP_RC23_REQUEST_TIME: u32 = 0x8025;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    // 0x0215 (ISO_15031_5_on_SAE_J1850_PWM), 0x0218 (ISO_15031_5_on_SAE_J1850_VPW),
    // 0x0217 (SAE_J2190_on_SAE_J1850_PWM), 0x021D (ISO_OBD_on_SAE_J1850 --
    // an alias row for the same ChannelProtocol as 0x021C, but resolved via
    // `protocol_default_params`'s name-based lookup to the distinct
    // `iso_obd_on_sae_j1850()` preset, not the `iso_15031_5_on_sae_j1850()`/
    // `sae_j2190_on_sae_j1850()` presets the other three resources below
    // use -- it is the only resource ID that reaches `iso_obd_on_sae_j1850`,
    // so it needs its own coverage here).
    for resource_id in [0x0215, 0x0218, 0x0217, 0x021D] {
        let cll_handle = create_cll(&mut client, resource_id, 5).await;
        assert_eq!(
            get_com_param_unum32(&mut client, cll_handle, CP_RC21_REQUEST_TIME).await,
            0,
            "J1850 resource {resource_id:#06x} should default CP_RC21RequestTime to 0 per \
             Table B.19"
        );
        assert_eq!(
            get_com_param_unum32(&mut client, cll_handle, CP_RC23_REQUEST_TIME).await,
            0,
            "J1850 resource {resource_id:#06x} should default CP_RC23RequestTime to 0 per \
             Table B.19"
        );
        assert_eq!(
            get_com_param_unum32(&mut client, cll_handle, CP_TESTER_PRESENT_TIME).await,
            3_000_000,
            "J1850 resource {resource_id:#06x} should default CP_TesterPresentTime to \
             3000000 per Table B.19"
        );
    }

    server.shutdown().await;
}

/// Conformance-audit finding A2-13 (ISO 22900-2:2009(E) Table B.21, ADR-130):
/// `ISO_11898_3_DWFTCAN`'s `CP_Baudrate` default is 125k (the bus's own
/// physical limit), not the stale 500k this bus type previously carried, and
/// its `CP_CanBaudrateRecord` has no spec-defined default at all (unlike
/// `ISO_11898_2_DWCAN`/`SAE_J1939_11_DWCAN`) -- so `GetComParam` for it
/// before any `SetComParam` must still report the correct Bytefield oneof
/// variant (empty), not fall through to the generic `Unum32(0)` response a
/// prior Codex review round on this PR found `rpc_get_com_param` doing for
/// any Bytefield param left without a default.
///
/// Also covers a second Codex review round's follow-up on the same finding:
/// that getter-side fix alone would let `SetComParam(CP_CanBaudrateRecord,
/// Unum32(_))` (accepted with no type check) silently mask its own stored
/// value behind the empty-Bytefield response instead of ever surfacing the
/// mismatch, so `rpc_set_com_param` must reject a `Unum32` write for this
/// param outright.
#[tokio::test]
#[serial]
async fn create_com_logical_link_a2_13_ftcan_comparam_defaults() {
    const CP_CAN_BAUDRATE_RECORD: u32 = 0x80A5;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_and_bustype(j2534_0404::CAN, "iso_11898_3_dwftcan"),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE).await,
        125_000,
        "ISO_11898_3_DWFTCAN should default CP_Baudrate to 125000 per Table B.21, \
         the bus's own physical limit"
    );

    let response = client
        .get_com_param(vci_service_interface::GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(
                vci_service_interface::get_com_param_request::Param::ParamId(
                    CP_CAN_BAUDRATE_RECORD,
                ),
            ),
        })
        .await
        .expect("get_com_param(CP_CanBaudrateRecord) should succeed")
        .into_inner();
    assert_eq!(
        response.param_item.and_then(|p| p.param_data),
        Some(vci_service_interface::param_item::ParamData::Bytefield(
            Vec::new()
        )),
        "ISO_11898_3_DWFTCAN has no spec-defined CP_CanBaudrateRecord default (Table B.21), \
         so GetComParam before any SetComParam must report an empty Bytefield -- not the \
         generic Unum32(0) fallback, which would be the wrong oneof variant for a \
         Bytefield-typed param"
    );

    let status = client
        .set_com_param(vci_service_interface::SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(vci_service_interface::ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    CP_CAN_BAUDRATE_RECORD,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(vci_service_interface::param_item::ParamData::Unum32(
                    500_000,
                )),
            }),
        })
        .await
        .expect_err(
            "SetComParam(CP_CanBaudrateRecord, Unum32(_)) should be rejected -- this param is \
             Bytefield-typed, and accepting a Unum32 write here would let it silently mask \
             itself behind GetComParam's empty-Bytefield response for an unset default",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// Third Codex review round on A2-13/ADR-130's `CP_CanBaudrateRecord` fix: a
/// client that round-trips `GetComParam`'s empty-`Bytefield` response
/// straight back through `SetComParam` (a common naive save/restore pattern)
/// must not trip the `PDU_PC_BUSTYPE` `temp_param_update` guard
/// (`bustype_params_differ`) -- storing an explicit empty entry in Working
/// while Active has none at all would otherwise read as a real difference,
/// rejecting a subsequent `temp_param_update=1` COP with
/// `PDU_ERR_TEMPPARAM_NOT_ALLOWED` even though the client changed nothing.
#[tokio::test]
#[serial]
async fn set_com_param_empty_baudrate_record_round_trip_stays_temp_param_update_safe() {
    const CP_CAN_BAUDRATE_RECORD: u32 = 0x80A5;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_with_protocol_and_bustype(j2534_0404::CAN, "iso_11898_3_dwftcan"),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present");

    client
        .connect_com_logical_link(vci_service_interface::ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    // Promote the current Working set (CP_CanBaudrateRecord absent from
    // both Working and Active at this point) to Active via CoptUpdateparam,
    // so Working == Active on every BUSTYPE-class key before the round-trip
    // below -- isolating this test to the one difference under test.
    set_can_phys_req_id_and_promote(&mut client, cll_handle, 0x7E0).await;

    // Round-trip GetComParam's empty-Bytefield response straight back
    // through SetComParam, as a naive save/restore client would.
    set_com_param_bytes(&mut client, cll_handle, CP_CAN_BAUDRATE_RECORD, Vec::new()).await;

    // CoptStartcomm, like CoptSendrecv, is a temp-eligible COP type gated by
    // the same synchronous BUSTYPE guard (see
    // `bustype_guard_rejects_temp_param_update_for_sendrecv_and_startcomm`'s
    // rejecting counterpart of this exact call shape) -- using it here
    // avoids needing CAN addressing/expected-response setup irrelevant to
    // the guard behavior under test.
    let status = client
        .start_com_primitive(vci_service_interface::StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(cll_handle),
            cop_type: vci_service_interface::ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: Some(vci_service_interface::ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 0,
                num_receive_cycles: 0,
                temp_param_update: 1,
                expected_response_array: Vec::<vci_service_interface::ExpectedResponseData>::new(),
                tx_flag: None,
            }),
        })
        .await;
    assert!(
        status.is_ok(),
        "a client that round-trips GetComParam's empty CP_CanBaudrateRecord back through \
         SetComParam should not trip the BUSTYPE temp_param_update guard -- got {:?}",
        status.err()
    );

    server.shutdown().await;
}

/// Full round trip: `GetResourceIds` resolves "ISO15765" to the opaque table
/// resource ID 0x0206; `CreateComLogicalLink`/`ConnectComLogicalLink` use
/// that ID directly; `GetResourceStatus`/`GetConflictingResources` (fed the
/// same opaque ID back) must resolve it through the table rather than
/// comparing it against `ChannelProtocol::value()` directly.
#[tokio::test]
#[serial]
async fn get_resource_status_and_conflicting_resources_resolve_table_resource_id() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let resource_id = get_resource_ids(&mut client, by_protocol_name("ISO15765"))
        .await
        .into_iter()
        .next()
        .expect("ISO15765 should resolve to a resource id");
    assert_eq!(resource_id, 0x0206);

    let _cll_handle = create_and_connect_cll(
        &mut client,
        resource_id,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let status = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(resource_id)),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();
    let resource_status_data = status
        .resource_status
        .expect("resource_status should be present")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_status, 1,
        "the connected CLL should report resource 0x0206 as active"
    );
    assert_eq!(
        resource_status_data[0].resource_id, 0x0206,
        "a ResourceId query should echo back exactly the ID the caller passed"
    );

    // A `resource_name` query resolving unambiguously through the table
    // (here "ISO_15765_2" -> resource 0x0206, the only row carrying
    // ChannelProtocol::ISO15765) must echo that table resource ID, not the
    // raw J2534/ChannelProtocol value (6) the legacy `map_object_type_name`
    // mapping would have produced.
    let status_by_name = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceName(
                    "ISO_15765_2".to_string(),
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();
    let resource_status_data_by_name = status_by_name
        .resource_status
        .expect("resource_status should be present")
        .resource_status_data;
    assert_eq!(resource_status_data_by_name.len(), 1);
    assert_eq!(resource_status_data_by_name[0].resource_status, 1);
    assert_eq!(
        resource_status_data_by_name[0].resource_id, 0x0206,
        "a resource_name query resolving unambiguously through the table should echo that row's resource id"
    );

    // ADR-106: GetConflictingResources is a static resource-table (pin)
    // query, independent of the live CLL just connected above -- it does
    // not report resource 0x0206 (ISO_15765_2, on the DWCAN bus) as
    // conflicting with itself just because a CLL is using it. Its real
    // table conflicts are the SCI rows sharing pin 6 or pin 14
    // (0x021E/0x021F/0x0222/0x0223), plus (ADR-174/Phase 10) the new Honda
    // DIAG-H row (0x023B) sharing pin 14 -- clause 13.2.3 itself flags this
    // exact collision, since DIAG-H's only documented pins are 1 and 14,
    // the latter shared with dual-wire CAN's CAN-L -- plus (ADR-188/Phase 7
    // Stage 7a) the TP2.0 row (0x025F), which reuses dual-wire CAN's exact
    // pin 6/14 pair but on a distinct bus_type_id, so it is a real pin
    // conflict rather than spec-legal channel sharing; the other
    // DWCAN-family rows sharing its exact bus_type_id + dlc_pins are
    // spec-legal channel sharing, not conflicts.
    let conflicts = client
        .get_conflicting_resources(GetConflictingResourcesRequest {
            resource: Some(get_conflicting_resources_request::Resource::ResourceId(
                resource_id,
            )),
            input_module_list: Some(single_module_list()),
        })
        .await
        .expect("get_conflicting_resources should succeed")
        .into_inner();
    let conflict_data = conflicts
        .conflict_list
        .expect("conflict_list should be present")
        .resource_conflict_data;
    let mut conflict_ids: Vec<u32> = conflict_data.iter().map(|c| c.resource_id).collect();
    conflict_ids.sort_unstable();
    assert_eq!(
        conflict_ids,
        vec![0x021E, 0x021F, 0x0222, 0x0223, 0x023B, 0x025F],
        "resource 0x0206's static table conflicts are the SCI rows sharing pin 6/14, the \
         Honda DIAG-H row sharing pin 14, and the TP2.0 row sharing pin 6/14, not the \
         connected CLL itself"
    );
    assert!(
        conflict_data.iter().all(|c| c.module_handle
            == Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE
            })),
        "every conflict entry should echo the single supported module handle"
    );

    server.shutdown().await;
}

/// `"SAE_J2610_SCI"` matches all four SCI configuration rows
/// (0x021F-0x0222), which differ only by `config_name`/`ChannelProtocol` --
/// `CreateComLogicalLink` must reject this ambiguous name rather than
/// silently picking one (e.g. `SCI_A_ENGINE`, the first in table order). A
/// specific config name (e.g. `"SCI_B_TRANS"`) still resolves unambiguously.
#[tokio::test]
#[serial]
async fn create_com_logical_link_rejects_ambiguous_sci_resource_name() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::ResourceName(
                "SAE_J2610_SCI".to_string(),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err("an ambiguous resource name should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("SCI_A_ENGINE") && status.message().contains("SCI_B_TRANS"),
        "error message should list the available configurations: {}",
        status.message()
    );

    let cll_response = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::ResourceName(
                "SCI_B_TRANS".to_string(),
            )),
            cll_create_flag: None,
        })
        .await
        .expect("a specific SCI configuration name should still resolve unambiguously")
        .into_inner();
    assert!(cll_response.cll_handle.is_some());

    server.shutdown().await;
}

/// A table-only resource name (`"ISO_OBD_on_K_Line"`, resource 0x0213, no
/// legacy `map_protocol_name`/`map_object_type_name` alias) must resolve
/// through the resource table for `GetResourceStatus`, not the legacy
/// name mapping alone (which would report it inactive).
#[tokio::test]
#[serial]
async fn get_resource_status_resolves_table_only_resource_name() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle =
        create_and_connect_cll(&mut client, 0x0213, &[(j2534_0404::DATA_RATE, 10_400)]).await;

    let status = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceName(
                    "ISO_OBD_on_K_Line".to_string(),
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();
    let resource_status_data = status
        .resource_status
        .expect("resource_status should be present")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(
        resource_status_data[0].resource_status, 1,
        "the connected CLL should report the ISO_OBD_on_K_Line resource name as active"
    );
    assert_eq!(
        resource_status_data[0].resource_id, 0x0213,
        "a unique table-name match should echo that row's own resource id, not its \
         alias 0x0212's (both share the same ChannelProtocol)"
    );

    server.shutdown().await;
}

/// A3-1/A3-2 (ISO 22900-2 §9.4.8.5 Table 16): a `ResourceName` matching
/// neither the resources table nor a legacy protocol-name alias must be
/// rejected with `PDU_ERR_INVALID_PARAMETERS`, not silently echoed back as
/// `resource_id: 0` / `resource_status: 0` (indistinguishable from a real,
/// idle/unlocked resource).
#[tokio::test]
#[serial]
async fn get_resource_status_rejects_unrecognized_resource_name() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceName(
                    "not_a_real_resource_or_protocol".to_string(),
                )),
            }],
        })
        .await
        .expect_err("an unrecognized resource name should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrInvalidParameters as i32
    );

    server.shutdown().await;
}

/// `"SAE_J2610_SCI"` matches all four SCI configuration resource IDs
/// (0x0222-0x0225) directly by name; `GetResourceStatus` must echo the
/// *active* one's own resource id (0x0225, `SCI_B_TRANS`), not the first in
/// table order (0x0222, `SCI_A_ENGINE`), since the echo is per-status-entry
/// here and should prefer whichever configuration the caller actually has
/// a link on.
#[tokio::test]
#[serial]
async fn get_resource_status_ambiguous_sci_name_echoes_the_active_configurations_id() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle =
        create_and_connect_cll(&mut client, 0x0225, &[(j2534_0404::DATA_RATE, 7_812)]).await;

    let status = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceName(
                    "SAE_J2610_SCI".to_string(),
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();
    let resource_status_data = status
        .resource_status
        .expect("resource_status should be present")
        .resource_status_data;
    assert_eq!(resource_status_data.len(), 1);
    assert_eq!(resource_status_data[0].resource_status, 1);
    assert_eq!(
        resource_status_data[0].resource_id, 0x0225,
        "the ambiguous SCI name should echo the id of the configuration actually in use"
    );

    server.shutdown().await;
}

/// Pin-driven selection (spec correction): `dlc_pin_data` narrows an
/// otherwise-ambiguous `RscData` protocol_name match to exactly one
/// configuration, whose `hw_protocol_override` (or, for the bare
/// `SAE_J2610_SCI` rows, own `ChannelProtocol`) is what actually connects --
/// observable via the resulting `PassThruConnect`/message protocol id, since
/// none of the four configurations' `protocol_name` differs.
#[tokio::test]
#[serial]
async fn create_com_logical_link_narrows_ambiguous_sci_name_by_typed_pins() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Bare "SAE_J2610_SCI" + SCI_A_TRANS's wiring (14, TX)/(7, RX) narrows to
    // exactly the SCI_A_TRANS configuration (resource 0x0223).
    let cll_handle = create_rsc_data_cll(
        &mut client,
        by_protocol_name_with_pins("SAE_J2610_SCI", &[(14, "TX"), (7, "RX")]),
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 7_812).await;
    client
        .connect_com_logical_link(vci_service_interface::ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");
    send_data(&mut client, cll_handle, vec![0x01, 0x02], vec![]).await;
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::SCI_A_TRANS
    );

    server.shutdown().await;
}

/// Same pin-driven narrowing, but for `"SAE_J2610_on_SAE_J2610_SCI"` (resource
/// 0x021F): all four configurations there share one `ChannelProtocol`, so
/// only `hw_protocol_override` (selected by pins) distinguishes them.
#[tokio::test]
#[serial]
async fn create_com_logical_link_narrows_ambiguous_j2610_on_name_by_typed_pins() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle = create_rsc_data_cll(
        &mut client,
        by_protocol_name_with_pins("SAE_J2610_on_SAE_J2610_SCI", &[(14, "TX"), (7, "RX")]),
    )
    .await;
    set_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE, 7_812).await;
    client
        .connect_com_logical_link(vci_service_interface::ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");
    send_data(&mut client, cll_handle, vec![0x01, 0x02], vec![]).await;
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::SCI_A_TRANS
    );

    server.shutdown().await;
}

/// Without any `dlc_pin_data`, `"SAE_J2610_on_SAE_J2610_SCI"` is ambiguous
/// across its four `hw_protocol_override` variants -- same rejection
/// `"SAE_J2610_SCI"` already gets, extended to cover rows sharing one
/// `ChannelProtocol` but differing in hardware override.
#[tokio::test]
#[serial]
async fn create_com_logical_link_rejects_ambiguous_j2610_on_name_without_pins() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                by_protocol_name_with_pins("SAE_J2610_on_SAE_J2610_SCI", &[]),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err("an ambiguous resource name with no pin data should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// A pin set that matches none of a name's candidate rows is rejected, not
/// silently treated as "no pin constraint".
#[tokio::test]
#[serial]
async fn create_com_logical_link_rejects_pins_matching_no_configuration() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                by_protocol_name_with_pins("SAE_J2610_SCI", &[(2, "PLUS")]),
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err("a pin set matching no configuration of this name should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// Codex review follow-up on the J1850 auto-detect hw-flavor fix: confirm
/// the same "TX validation must follow the hardware, not service identity"
/// principle holds for the other divergence this table introduced --
/// `SAE_J2610_on_SAE_J2610_SCI`'s `hw_protocol_override`. Unlike J1850
/// VPW/PWM, `tx_message_size_range` treats every native SCI protocol id
/// (`SCI_A_ENGINE`..`SCI_B_TRANS`) identically (1..=4128, `protocol.rs`), so
/// there is no size-range divergence to exercise here -- this pins that a
/// row using the override (0x0221, `hw_protocol_override = SCI_B_TRANS`)
/// still connects and sends successfully with the (identical either way)
/// SCI limits, and that the wire protocol id is the override, not the
/// shared `SAE_J2610_ON_SAE_J2610_SCI` identity (0x0160/`SCI_MODE`).
#[tokio::test]
#[serial]
async fn j2610_on_hw_override_row_validates_and_sends_with_sci_tx_limits() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let cll_handle =
        create_and_connect_cll(&mut client, 0x0221, &[(j2534_0404::DATA_RATE, 7_812)]).await;
    send_data(&mut client, cll_handle, vec![0x01, 0x02], vec![]).await;
    assert_eq!(
        server.backdoor.written_protocol_id(MOCK_CHANNEL_ID, 0),
        j2534_0404::SCI_B_TRANS,
        "the hw_protocol_override should reach PassThruWriteMsgs, not SCI_MODE"
    );

    server.shutdown().await;
}

/// The four `SAE_J2610_on_SAE_J2610_SCI` rows (0x021E..0x0221) share one
/// `ChannelProtocol` but override distinct hardware protocol IDs
/// (`hw_protocol_override`), which becomes the connected link's own
/// `hw_protocol_id`. `GetResourceStatus` resolution must key on both
/// `protocol` *and* `hw_protocol_override`, not `protocol` alone --
/// otherwise connecting 0x0221 (`SCI_B_TRANS`) would falsely report every
/// other 0x0160 row (e.g. 0x021E, `SCI_A_ENGINE`) as active too.
/// `GetConflictingResources` (ADR-106) is unaffected by any of this live
/// connection state -- it is a static pin-table query, asserted separately
/// below.
#[tokio::test]
#[serial]
async fn get_resource_status_and_conflicts_disambiguate_j2610_on_hw_override_rows() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle =
        create_and_connect_cll(&mut client, 0x0221, &[(j2534_0404::DATA_RATE, 7_812)]).await;

    // GetResourceStatus(0x0221) -- the connected row -- reports active.
    let status_0221 = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(0x0221)),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner()
        .resource_status
        .expect("resource_status should be present")
        .resource_status_data;
    assert_eq!(status_0221.len(), 1);
    assert_eq!(
        status_0221[0].resource_status, 1,
        "resource 0x0221 (the connected SCI_B_TRANS override) should report active"
    );

    // GetResourceStatus(0x021E) -- a different SCI_A_ENGINE override sharing
    // the same ChannelProtocol -- must NOT report active.
    let status_021e = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(0x021E)),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner()
        .resource_status
        .expect("resource_status should be present")
        .resource_status_data;
    assert_eq!(status_021e.len(), 1);
    assert_eq!(
        status_021e[0].resource_status, 0,
        "resource 0x021E (SCI_A_ENGINE) shares ChannelProtocol with the connected 0x0221 \
         (SCI_B_TRANS) but is a different hardware configuration, so it must not report active"
    );

    // ADR-106: GetConflictingResources(0x0221) is a static pin-table query,
    // unaffected by the live CLL connected above -- 0x0221's (SCI_B_TRANS,
    // pins 9/15) real table conflicts are the K-line family rows sharing
    // pin 15 (0x020B-0x0214) plus every other SAE_J2610_UART row: they all
    // share the one physical SCI controller with 0x0221 (`same_bus`), even
    // though its own SCI_A_* siblings share no *pin* with it. The bare
    // SAE_J2610_SCI SCI_B_TRANS row (0x0225) is the identical electrical
    // configuration on that controller, so it alone stays exempt as
    // spec-legal sharing, not a conflict. ADR-168/Phase 6 adds a new pin-9
    // overlap: every FTCAN row (0x0230-0x0239) also wires pin 9 (its own
    // two-pin default, clause 20.2.1), so they now conflict with 0x0221 too.
    // ADR-189/Phase 8 adds one more pin-9 overlap: the new GM UART row
    // (0x0260) wires pin 9 too (its own single-pin default, clause 11.2.2),
    // so it now conflicts with 0x0221 as well.
    // ADR-194/Phase 16 (Codex review, PR #102) adds one more pin-9 overlap:
    // Ethernet_NDIS (0x0261) does not type pin 9 into its own `dlc_pins`
    // (only Option 1's Tx pins, 3/11), but clause 24 Table 103's Option 2
    // alternate Tx(-) pin IS 9 -- `rows_conflict`'s dedicated Ethernet_NDIS
    // alternate-pin check reports this conflict even though a plain
    // `dlc_pins` overlap would miss it, since a real connection could stage
    // `CP_NdisPinOption = 2` and genuinely contend for pin 9.
    let conflicts = client
        .get_conflicting_resources(GetConflictingResourcesRequest {
            resource: Some(get_conflicting_resources_request::Resource::ResourceId(
                0x0221,
            )),
            input_module_list: Some(single_module_list()),
        })
        .await
        .expect("get_conflicting_resources should succeed")
        .into_inner()
        .conflict_list
        .expect("conflict_list should be present")
        .resource_conflict_data;
    let mut conflict_ids: Vec<u32> = conflicts.iter().map(|c| c.resource_id).collect();
    conflict_ids.sort_unstable();
    assert_eq!(
        conflict_ids,
        vec![
            0x020B, 0x020C, 0x020D, 0x020E, 0x020F, 0x0210, 0x0211, 0x0212, 0x0213, 0x0214, 0x021E,
            0x021F, 0x0220, 0x0222, 0x0223, 0x0224, 0x0230, 0x0231, 0x0232, 0x0233, 0x0234, 0x0235,
            0x0236, 0x0237, 0x0238, 0x0239, 0x0260, 0x0261,
        ],
        "0x0221's static table conflicts are the K-line rows sharing pin 15, plus every other \
         SAE_J2610_UART row (same shared SCI controller) other than its identically-wired \
         SAE_J2610_SCI sibling (0x0225, spec-legal sharing), plus every FTCAN row and the GM UART \
         row sharing pin 9, plus Ethernet_NDIS via its Option 2 alternate Tx(-) pin (also 9, ADR-194); \
         not the live CLL connected above"
    );

    // A `resource_name` query for the shared name must echo 0x0221 too,
    // since that is the specific configuration actually connected.
    let status_by_name = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceName(
                    "SAE_J2610_on_SAE_J2610_SCI".to_string(),
                )),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner()
        .resource_status
        .expect("resource_status should be present")
        .resource_status_data;
    assert_eq!(status_by_name.len(), 1);
    assert_eq!(status_by_name[0].resource_status, 1);
    assert_eq!(
        status_by_name[0].resource_id, 0x0221,
        "the shared-ChannelProtocol name should echo the id of the hardware configuration \
         actually connected, not the first in table order"
    );

    server.shutdown().await;
}

/// Pin narrowing that reduces a name's candidate rows to zero is an error,
/// even for a name that (absent pin data) would otherwise resolve
/// unambiguously to a single row -- `ISO_15765_2` names exactly one row
/// (0x0206, pins 6/HI and 14/LOW), so a bare `dlc_pin_data` entry naming a
/// pin number that row doesn't have (pin 7, no type) narrows the match to
/// zero configurations rather than being silently ignored (pins were
/// previously ignored once a name resolved to a single row; this is an
/// intentional tightening -- see ADR-069's Consequences and
/// `docs/protocol-mapping.md`'s `CreateComLogicalLink` section).
#[tokio::test]
#[serial]
async fn create_com_logical_link_rejects_pin_narrowing_a_unique_name_to_zero_rows() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: vec![PinData {
                        dlc_pin_number: 7,
                        dlc_pin_type: None,
                    }],
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolName(
                        "ISO_15765_2".to_string(),
                    )),
                },
            )),
            cll_create_flag: None,
        })
        .await
        .expect_err(
            "dlc_pin_data naming a pin resource 0x0206 doesn't have should be rejected, \
             not silently ignored",
        );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// Coverage for `GetResourceIds`' pin-narrowing rows: pin 6 typed `TX` and
/// pin 7 typed `RX` together match only the SCI_A_ENGINE configurations
/// (0x021E, 0x0222) -- the CAN rows also have a pin 6, but typed `HI`, so a
/// `TX`-typed pin 6 excludes them even though the bare pin number matches.
#[tokio::test]
#[serial]
async fn filters_by_pin_set_excludes_can_rows_with_same_pin_number_but_different_type() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let resource_data = ResourceData {
        dlc_pin_data: vec![
            PinData {
                dlc_pin_number: 6,
                dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                    "TX".to_string(),
                )),
            },
            PinData {
                dlc_pin_number: 7,
                dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                    "RX".to_string(),
                )),
            },
        ],
        bus_type: None,
        protocol: None,
    };

    let ids = get_resource_ids(&mut client, resource_data).await;
    assert_eq!(
        ids,
        vec![0x021E, 0x0222],
        "pin 6 typed TX excludes the CAN rows (pin 6 there is typed HI, not TX)"
    );

    server.shutdown().await;
}

// ADR-106: `GetConflictingResources` is a static resource-table query,
// callable before any `ComLogicalLink` exists -- these tests never create
// one.

/// §9.4.26.2 a) NULL-pointer-equivalent check: `resource` (the oneof)
/// entirely unset is rejected up front.
#[tokio::test]
#[serial]
async fn get_conflicting_resources_requires_resource_selector() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .get_conflicting_resources(GetConflictingResourcesRequest {
            resource: None,
            input_module_list: Some(single_module_list()),
        })
        .await
        .expect_err("an unset resource selector should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrInvalidParameters as i32
    );

    server.shutdown().await;
}

/// §9.4.26.2 a) `pInputModuleList` NULL-pointer-equivalent check: entirely
/// unset is rejected, even for an otherwise-valid `resource_id`.
#[tokio::test]
#[serial]
async fn get_conflicting_resources_requires_input_module_list() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .get_conflicting_resources(GetConflictingResourcesRequest {
            resource: Some(get_conflicting_resources_request::Resource::ResourceId(
                0x0206,
            )),
            input_module_list: None,
        })
        .await
        .expect_err("a missing input_module_list should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrInvalidParameters as i32
    );

    server.shutdown().await;
}

/// A *present* but empty `input_module_list` is a valid call with nothing to
/// check against -- not an error, just an empty result.
#[tokio::test]
#[serial]
async fn get_conflicting_resources_empty_module_list_returns_empty_result() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let conflicts = client
        .get_conflicting_resources(GetConflictingResourcesRequest {
            resource: Some(get_conflicting_resources_request::Resource::ResourceId(
                0x0206,
            )),
            input_module_list: Some(ModuleItem {
                module_data: vec![],
            }),
        })
        .await
        .expect("an empty (but present) input_module_list should succeed")
        .into_inner()
        .conflict_list
        .expect("conflict_list should be present")
        .resource_conflict_data;
    assert!(
        conflicts.is_empty(),
        "an empty input_module_list has nothing to check against"
    );

    server.shutdown().await;
}

/// Every `input_module_list` entry is validated against the single
/// supported module handle -- fail closed on the first bad entry rather
/// than silently skipping it.
#[tokio::test]
#[serial]
async fn get_conflicting_resources_rejects_unknown_module_handle() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .get_conflicting_resources(GetConflictingResourcesRequest {
            resource: Some(get_conflicting_resources_request::Resource::ResourceId(
                0x0206,
            )),
            input_module_list: Some(ModuleItem {
                module_data: vec![ModuleData {
                    module_handle: Some(ModuleHandle {
                        module_handle: MOCK_MODULE_HANDLE + 1,
                    }),
                    ..Default::default()
                }],
            }),
        })
        .await
        .expect_err("an unrecognized module_handle should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrInvalidHandle as i32
    );

    server.shutdown().await;
}

/// An `input_module_list` entry with `module_handle` unset (as opposed to
/// the whole list being absent, or present with a wrong handle value) is
/// also rejected -- `require_module_handle(None)` fails the same way it
/// does for every other RPC that validates a module handle.
#[tokio::test]
#[serial]
async fn get_conflicting_resources_rejects_entry_with_missing_module_handle() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .get_conflicting_resources(GetConflictingResourcesRequest {
            resource: Some(get_conflicting_resources_request::Resource::ResourceId(
                0x0206,
            )),
            input_module_list: Some(ModuleItem {
                module_data: vec![ModuleData {
                    module_handle: None,
                    ..Default::default()
                }],
            }),
        })
        .await
        .expect_err("a module_data entry with no module_handle should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// A raw/extended `ChannelProtocol` value with no resource-table row
/// (ADR-069 legacy fallback, still a valid `CreateComLogicalLink` input)
/// resolves to an empty queried set for `GetConflictingResources` -- no
/// pin/bus metadata exists to compute conflicts from, so this returns an
/// empty result rather than an error or a protocol-only fallback match.
#[tokio::test]
#[serial]
async fn get_conflicting_resources_unmapped_legacy_resource_id_returns_empty_result() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let conflicts = client
        .get_conflicting_resources(GetConflictingResourcesRequest {
            // ChannelProtocol::ISO_11783_12_ON_ISO_11783_5 -- a raw/extended
            // protocol value with no resources-table row.
            resource: Some(get_conflicting_resources_request::Resource::ResourceId(
                0x0110,
            )),
            input_module_list: Some(single_module_list()),
        })
        .await
        .expect("an unmapped legacy resource_id should succeed")
        .into_inner()
        .conflict_list
        .expect("conflict_list should be present")
        .resource_conflict_data;
    assert!(
        conflicts.is_empty(),
        "a resource_id with no table row has no pin/bus metadata to compute conflicts from"
    );

    server.shutdown().await;
}

/// A `resource_name` that matches several table rows (`"SAE_J2610_SCI"`
/// matches all four SCI configurations, 0x0222-0x0225) must union each
/// row's conflicts and dedup by resource_id, not just report one row's
/// conflicts -- e.g. 0x021E conflicts with three of the four query rows
/// (0x0223/0x0224 via pin overlap, 0x0225 via the shared SAE_J2610_UART
/// controller even though it shares no pin), but not with the fourth
/// (0x0222, identical electrical configuration, spec-legal sharing), so it
/// must still show up exactly once, not four times. ADR-170/Phase 9 adds a
/// new pin-7 overlap: 0x0222 (SCI_A_ENGINE, pin 7 RX), 0x0223 (SCI_A_TRANS,
/// pin 7 RX), and 0x0224 (SCI_B_ENGINE, pin 7 RX) all share pin 7 with the
/// new UART Echo Byte row (0x023A, pin 7), so it now conflicts with the
/// union too (0x0225, which has no pin 7, does not add this conflict on its
/// own, but the union still includes it via the other three). ADR-174/Phase
/// 10 adds a new pin-14 overlap the same way: whichever of these four SCI
/// rows already shares pin 14 with the DWCAN family (0x0201-0x020A) also
/// shares it with the new Honda DIAG-H row (0x023B, pin 14), so 0x023B
/// joins the union too. ADR-188/Phase 7 Stage 7a adds one more: the query
/// row typed pin 6 TX (SCI_A_ENGINE, 0x0222) and the one typed pin 14 TX
/// (SCI_A_TRANS, 0x0223) each overlap the new TP2.0 row's pin 6/14 pair
/// (0x025F), so it joins the union too. ADR-189/Phase 8 adds one more: the
/// query row typed pin 9 TX (SCI_B_TRANS, 0x0221, the same row already
/// conflicting with every FTCAN row via pin 9) also overlaps the new GM
/// UART row's own pin 9 default (0x0260), so it joins the union too.
/// ADR-194/Phase 16 adds one more: the query row typed pin 12 TX
/// (SCI_B_ENGINE, 0x0224) overlaps the new Ethernet_NDIS row's own pin 12
/// (Rx+, 0x0261), so it joins the union too.
#[tokio::test]
#[serial]
async fn get_conflicting_resources_resource_name_unions_and_dedups_multiple_rows() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let conflicts = client
        .get_conflicting_resources(GetConflictingResourcesRequest {
            resource: Some(get_conflicting_resources_request::Resource::ResourceName(
                "SAE_J2610_SCI".to_string(),
            )),
            input_module_list: Some(single_module_list()),
        })
        .await
        .expect("get_conflicting_resources should succeed")
        .into_inner()
        .conflict_list
        .expect("conflict_list should be present")
        .resource_conflict_data;
    let mut conflict_ids: Vec<u32> = conflicts.iter().map(|c| c.resource_id).collect();
    let unique_count = {
        let mut sorted = conflict_ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        sorted.len()
    };
    assert_eq!(
        conflict_ids.len(),
        unique_count,
        "each conflicting resource id should be reported exactly once"
    );
    conflict_ids.sort_unstable();
    assert_eq!(
        conflict_ids,
        vec![
            0x0201, 0x0202, 0x0203, 0x0204, 0x0205, 0x0206, 0x0207, 0x0208, 0x0209, 0x020A, 0x020B,
            0x020C, 0x020D, 0x020E, 0x020F, 0x0210, 0x0211, 0x0212, 0x0213, 0x0214, 0x021E, 0x021F,
            0x0220, 0x0221, 0x0222, 0x0223, 0x0224, 0x0225, 0x0230, 0x0231, 0x0232, 0x0233, 0x0234,
            0x0235, 0x0236, 0x0237, 0x0238, 0x0239, 0x023A, 0x023B, 0x025F, 0x0260, 0x0261,
        ],
        "the union of all four SAE_J2610_SCI rows' conflicts, deduplicated -- includes every \
         other SAE_J2610_UART row (shared SCI controller) not identically wired to the query row \
         it's paired against, plus (ADR-168/Phase 6) every FTCAN row sharing pin 9 with \
         SCI_B_TRANS, plus (ADR-170/Phase 9) the new UART Echo Byte row sharing pin 7 with \
         SCI_A_ENGINE/SCI_A_TRANS/SCI_B_ENGINE, plus (ADR-188/Phase 7 Stage 7a) the new TP2.0 \
         row sharing pin 6 with SCI_A_ENGINE and pin 14 with SCI_A_TRANS, plus (ADR-189/Phase 8) \
         the new GM UART row sharing pin 9 with SCI_B_TRANS, plus (ADR-194/Phase 16) the new \
         Ethernet_NDIS row sharing pin 12 with SCI_B_ENGINE"
    );

    server.shutdown().await;
}

/// Codex review finding on PR #110 (ADR-107): the response `module_handle`
/// must identify which of the *requested* modules each conflict row applies
/// to, not a hardcoded `DEFAULT_MODULE_HANDLE` -- verified with a
/// multi-module config and a two-entry `input_module_list`. Per ISO 22900-2
/// §9.4.26.2 b) (determine every resource conflict among the modules named in
/// pInputModuleList) and §11.1.4.9's `PDU_RSC_CONFLICT_DATA.hMod` (the handle
/// of the protocol module that has the conflict), the static
/// resource-table conflict set (ADR-106, identical across modules since the
/// table isn't itself partitioned per module) must be reported once per
/// queried module, each row tagged with that module's own handle.
#[tokio::test]
#[serial]
async fn get_conflicting_resources_tags_each_row_with_its_own_queried_module_handle() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    let conflicts = client
        .get_conflicting_resources(GetConflictingResourcesRequest {
            resource: Some(get_conflicting_resources_request::Resource::ResourceId(
                0x0206,
            )),
            input_module_list: Some(ModuleItem {
                module_data: vec![
                    ModuleData {
                        module_handle: Some(ModuleHandle { module_handle: 1 }),
                        ..Default::default()
                    },
                    ModuleData {
                        module_handle: Some(ModuleHandle { module_handle: 2 }),
                        ..Default::default()
                    },
                ],
            }),
        })
        .await
        .expect("get_conflicting_resources should succeed")
        .into_inner()
        .conflict_list
        .expect("conflict_list should be present")
        .resource_conflict_data;

    let expected_resource_ids = vec![0x021Eu32, 0x021F, 0x0222, 0x0223, 0x023B, 0x025F];

    for handle in [1u32, 2u32] {
        let mut ids: Vec<u32> = conflicts
            .iter()
            .filter(|c| {
                c.module_handle
                    == Some(ModuleHandle {
                        module_handle: handle,
                    })
            })
            .map(|c| c.resource_id)
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids, expected_resource_ids,
            "module {handle} should get the full same table conflict set, tagged with its own \
             handle rather than a hardcoded default"
        );
    }
    assert_eq!(
        conflicts.len(),
        expected_resource_ids.len() * 2,
        "each of the two queried modules should get its own row per conflicting resource -- not \
         a single set collapsed onto DEFAULT_MODULE_HANDLE"
    );

    server.shutdown().await;
}

/// design-advisor follow-up on the same PR #110 edge-case-hunter pass as the
/// test above: `input_module_list` is not deduplicated by handle, so naming
/// the same `module_handle` twice is a plain pass-through, not a bug -- the
/// response gets that handle's conflict rows twice too.
#[tokio::test]
#[serial]
async fn get_conflicting_resources_does_not_dedup_a_repeated_module_handle_in_the_query() {
    let server = TestServer::start_with_modules(&[("Bench 1", "USB:1")]).await;
    let mut client = server.client().await;

    let conflicts = client
        .get_conflicting_resources(GetConflictingResourcesRequest {
            resource: Some(get_conflicting_resources_request::Resource::ResourceId(
                0x0206,
            )),
            input_module_list: Some(ModuleItem {
                module_data: vec![
                    ModuleData {
                        module_handle: Some(ModuleHandle {
                            module_handle: MOCK_MODULE_HANDLE,
                        }),
                        ..Default::default()
                    },
                    ModuleData {
                        module_handle: Some(ModuleHandle {
                            module_handle: MOCK_MODULE_HANDLE,
                        }),
                        ..Default::default()
                    },
                ],
            }),
        })
        .await
        .expect("get_conflicting_resources should succeed")
        .into_inner()
        .conflict_list
        .expect("conflict_list should be present")
        .resource_conflict_data;

    let expected_resource_ids = [0x021Eu32, 0x021F, 0x0222, 0x0223, 0x023B, 0x025F];
    assert_eq!(
        conflicts.len(),
        expected_resource_ids.len() * 2,
        "a module handle named twice in the query gets its conflict rows emitted twice -- the \
         input list is not deduplicated, so neither is the output"
    );

    server.shutdown().await;
}

/// Codex review finding on PR #110 (ADR-107): a resource can only be
/// "active" for the module that is actually open right now
/// (single-open-device model) -- querying an unopened (but configured,
/// in-range) module must not scan `logical_links`, which belongs entirely
/// to whichever module IS open, regardless of which module the query names.
#[tokio::test]
#[serial]
async fn get_resource_status_only_reports_active_for_the_actually_open_module() {
    let server =
        TestServer::start_with_modules(&[("Bench 1", "USB:1"), ("Bench 2", "USB:2")]).await;
    let mut client = server.client().await;

    // `create_and_connect_cll` always targets `MOCK_MODULE_HANDLE` (1),
    // opening module 1's device and connecting a live CAN link on it.
    let resource_id = get_resource_ids(&mut client, by_protocol_name("ISO15765"))
        .await
        .into_iter()
        .next()
        .expect("ISO15765 should resolve to a resource id");
    let _cll_handle = create_and_connect_cll(
        &mut client,
        resource_id,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // Addressed to module 2 (configured, in range, but not the open one):
    // must report inactive even though the same resource IS active on
    // module 1.
    let status_module2 = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle { module_handle: 2 }),
                resource: Some(module_and_resource_id::Resource::ResourceId(resource_id)),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();
    let data_module2 = status_module2
        .resource_status
        .expect("resource_status should be present")
        .resource_status_data;
    assert_eq!(data_module2.len(), 1);
    assert_eq!(
        data_module2[0].resource_status, 0,
        "module 2 is not the open module, so the resource must report inactive even though it's \
         active on module 1"
    );

    // A sibling query addressed to module 1 (the actually-open one) for the
    // same resource still correctly reports active.
    let status_module1 = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(resource_id)),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();
    let data_module1 = status_module1
        .resource_status
        .expect("resource_status should be present")
        .resource_status_data;
    assert_eq!(data_module1.len(), 1);
    assert_eq!(
        data_module1[0].resource_status, 1,
        "module 1 is the actually-open module with the active CAN link"
    );

    server.shutdown().await;
}

/// A2-3 (`iso22900-2-conformance-audit.md`), ADR-127: §9.4.9.2 c) marks a
/// resource "in use" in the resource table at `CreateComLogicalLink` time,
/// before `ConnectComLogicalLink` is ever called -- `resource_status` bit 0
/// must reflect that from creation onward, not only once `connected` is
/// true.
#[tokio::test]
#[serial]
async fn get_resource_status_reports_in_use_for_a_created_but_unconnected_cll() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let resource_id = get_resource_ids(&mut client, by_protocol_name("ISO15765"))
        .await
        .into_iter()
        .next()
        .expect("ISO15765 should resolve to a resource id");

    // Created but never connected.
    let _cll_handle = create_cll(&mut client, resource_id, 1).await;

    let status = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(resource_id)),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();
    let data = status
        .resource_status
        .expect("resource_status should be present")
        .resource_status_data;
    assert_eq!(data.len(), 1);
    assert_ne!(
        data[0].resource_status & 0x01,
        0,
        "an unconnected but created CLL must still report bit 0 (in use), per §9.4.9.2 c)"
    );

    server.shutdown().await;
}

/// A2-3 (`iso22900-2-conformance-audit.md`), ADR-127: `resource_status` bits
/// 2/3 (Table D.1) must surface a `LockResource`-held
/// `LOCK_PHYSICAL_TX_QUEUE`/`LOCK_PHYSICAL_COM_PARAMS` bit for ANY CLL
/// sharing the physical resource, not just the specific CLL that holds the
/// lock -- `GetResourceStatus` takes a module + resource, never a CLL
/// handle, so a sibling's query for the same resource must see the same
/// bits.
#[tokio::test]
#[serial]
async fn get_resource_status_surfaces_a_siblings_held_lock_bits() {
    const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;
    const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

    let server = TestServer::start().await;
    let mut client = server.client().await;

    let resource_id = get_resource_ids(&mut client, by_protocol_name("ISO15765"))
        .await
        .into_iter()
        .next()
        .expect("ISO15765 should resolve to a resource id");

    let cll_handle = create_and_connect_cll(
        &mut client,
        resource_id,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    client
        .lock_resource(vci_service_interface::LockResourceRequest {
            cll_handle: Some(cll_handle),
            lock_mask: LOCK_PHYSICAL_TX_QUEUE | LOCK_PHYSICAL_COM_PARAMS,
        })
        .await
        .expect("lock_resource should succeed");

    // Queried by resource, not by CLL handle -- `GetResourceStatus` has no
    // CLL-handle parameter at all.
    let status = client
        .get_resource_status(GetResourceStatusRequest {
            resources: vec![ModuleAndResourceId {
                module_handle: Some(ModuleHandle {
                    module_handle: MOCK_MODULE_HANDLE,
                }),
                resource: Some(module_and_resource_id::Resource::ResourceId(resource_id)),
            }],
        })
        .await
        .expect("get_resource_status should succeed")
        .into_inner();
    let data = status
        .resource_status
        .expect("resource_status should be present")
        .resource_status_data;
    assert_eq!(data.len(), 1);
    let resource_status = data[0].resource_status;
    assert_ne!(
        resource_status & 0x04,
        0,
        "bit 2 (TX-queue lock) must be set once LOCK_PHYSICAL_TX_QUEUE is held on the resource"
    );
    assert_ne!(
        resource_status & 0x08,
        0,
        "bit 3 (ComParams lock) must be set once LOCK_PHYSICAL_COM_PARAMS is held on the resource"
    );
    assert_ne!(
        resource_status & 0x01,
        0,
        "the connected CLL still makes the resource in-use (bit 0)"
    );

    server.shutdown().await;
}

/// Edge-case-hunter regression (A2-3 follow-up, ADR-127): `resource_status`
/// must describe the SAME resource as the response's own echoed
/// `resource_id`, even for an ambiguous `"SAE_J2610_SCI"` name query (four
/// candidate rows, 0x0222-0x0225). Confirmed live repro before this fix:
/// creating (not connecting) a CLL on 0x0225 alone produced
/// `resource_id=0x0222` (first table row, the tie-break when nothing is
/// connected) paired with `resource_status`'s bit 0 set -- borrowed from
/// 0x0225's CLL -- incorrectly telling the caller that resource 0x0222 (which
/// has zero CLLs) is in use.
#[tokio::test]
#[serial]
async fn get_resource_status_ambiguous_name_reports_status_for_the_same_row_it_echoes() {
    // Negative case: a CLL created (not connected) on 0x0225 must not leak
    // "in use" onto the echoed 0x0222 (first table row, since nothing is
    // connected so there is no active-link tie-break).
    {
        let server = TestServer::start().await;
        let mut client = server.client().await;

        let _cll_handle = create_cll(&mut client, 0x0225, 1).await;

        let status = client
            .get_resource_status(GetResourceStatusRequest {
                resources: vec![ModuleAndResourceId {
                    module_handle: Some(ModuleHandle {
                        module_handle: MOCK_MODULE_HANDLE,
                    }),
                    resource: Some(module_and_resource_id::Resource::ResourceName(
                        "SAE_J2610_SCI".to_string(),
                    )),
                }],
            })
            .await
            .expect("get_resource_status should succeed")
            .into_inner();
        let data = status
            .resource_status
            .expect("resource_status should be present")
            .resource_status_data;
        assert_eq!(data.len(), 1);
        assert_eq!(
            data[0].resource_id, 0x0222,
            "with nothing connected, the ambiguous name still echoes the first table row"
        );
        assert_eq!(
            data[0].resource_status & 0x01,
            0,
            "the echoed 0x0222 row has no CLL of its own -- 0x0225's created-but-unconnected \
             CLL must not leak its in-use status onto a different echoed resource id"
        );

        server.shutdown().await;
    }

    // Positive case: a CLL connected on 0x0225 makes it the active-link
    // tie-break, so both the echoed `resource_id` and `resource_status`'s
    // bit 0 must agree on 0x0225.
    {
        let server = TestServer::start().await;
        let mut client = server.client().await;

        let _cll_handle =
            create_and_connect_cll(&mut client, 0x0225, &[(j2534_0404::DATA_RATE, 7_812)]).await;

        let status = client
            .get_resource_status(GetResourceStatusRequest {
                resources: vec![ModuleAndResourceId {
                    module_handle: Some(ModuleHandle {
                        module_handle: MOCK_MODULE_HANDLE,
                    }),
                    resource: Some(module_and_resource_id::Resource::ResourceName(
                        "SAE_J2610_SCI".to_string(),
                    )),
                }],
            })
            .await
            .expect("get_resource_status should succeed")
            .into_inner();
        let data = status
            .resource_status
            .expect("resource_status should be present")
            .resource_status_data;
        assert_eq!(data.len(), 1);
        assert_eq!(
            data[0].resource_id, 0x0225,
            "the connected configuration should be the one echoed"
        );
        assert_ne!(
            data[0].resource_status & 0x01,
            0,
            "the echoed 0x0225 row is the same one that's actually connected"
        );

        server.shutdown().await;
    }
}

async fn create_rsc_data_cll(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    resource_data: ResourceData,
) -> vci_service_interface::ComLogicalLinkHandle {
    client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                resource_data,
            )),
            cll_create_flag: None,
        })
        .await
        .expect("create_com_logical_link should succeed")
        .into_inner()
        .cll_handle
        .expect("cll_handle should be present")
}
